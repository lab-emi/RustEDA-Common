// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
//! Bounded explicit-interface metadata, rejecting ambiguous duplicate keys.
use std::collections::BTreeMap;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};

struct Unique<T, const LIMIT: usize>(BTreeMap<String, T>);
impl<'de, T: Deserialize<'de>, const LIMIT: usize> Deserialize<'de> for Unique<T, LIMIT> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor<T, const LIMIT: usize>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for UniqueVisitor<T, LIMIT> {
            type Value = Unique<T, LIMIT>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a bounded map with unique interface names")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut out = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, T>()? {
                    if out.len() >= LIMIT {
                        return Err(de::Error::custom("cell-bus schema entry budget exceeded"));
                    }
                    if out.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate cell-bus schema key"));
                    }
                }
                Ok(Unique(out))
            }
        }
        deserializer.deserialize_map(UniqueVisitor::<T, LIMIT>(PhantomData))
    }
}
pub(super) fn parse(bytes: &[u8]) -> Result<super::frozen::CellBusSchemas, serde_json::Error> {
    let map: Unique<Unique<[i32; 2], 4096>, 256> = serde_json::from_slice(bytes)?;
    Ok(map
        .0
        .into_iter()
        .map(|(name, ports)| (name, ports.0))
        .collect())
}
#[cfg(test)]
mod tests {
    #[test]
    fn duplicate_interface_keys_are_rejected() {
        assert!(super::parse(br#"{"M":{"A":[3,0],"A":[0,3]}}"#).is_err());
        assert!(super::parse(br#"{"M":{},"M":{"A":[3,0]}}"#).is_err());
        assert!(super::parse(br#"{"M":{"A":[3,0],"Q":[0,3]}}"#).is_ok());
    }
}
