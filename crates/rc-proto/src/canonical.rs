//! Canonical JSON: sorted object keys, compact (no whitespace).
//!
//! Used for any prefix-stable byte sequence — the tools array above all — so
//! that re-serializing the same value always yields identical bytes regardless
//! of `HashMap` iteration order, build, or session (§4.6). The M1
//! `prefix_stability` integration test relies on this; without it, a reordered
//! schema would silently zero the cache hit rate against a prefix-caching
//! router. Doing it from M0 means the discipline is in place before anything
//! depends on it.

use serde::Serialize;
use serde_json::Value;

/// Serialize `value` to canonical JSON (sorted keys, compact).
pub fn to_string<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let v = serde_json::to_value(value)?;
    Ok(canonicalize(&v).to_string())
}

/// Recursively canonicalize a `serde_json::Value` (sort object keys).
pub fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(entries.len());
            for (k, val) in entries {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Schema {
        z: u32,
        a: u32,
        m: Nested,
    }

    #[derive(Serialize)]
    struct Nested {
        y: u32,
        b: u32,
    }

    #[test]
    fn sorts_keys_recursively_and_is_compact() {
        let s = Schema { z: 1, a: 2, m: Nested { y: 3, b: 4 } };
        let out = to_string(&s).unwrap();
        assert_eq!(out, r#"{"a":2,"m":{"b":4,"y":3},"z":1}"#);
    }

    #[test]
    fn is_deterministic_across_construction_orders() {
        // Two objects built with different insertion orders serialize identically.
        let mut a = serde_json::Map::new();
        a.insert("second".to_string(), Value::Bool(true));
        a.insert("first".to_string(), Value::Bool(false));

        let mut b = serde_json::Map::new();
        b.insert("first".to_string(), Value::Bool(false));
        b.insert("second".to_string(), Value::Bool(true));

        let canon_a = to_string(&Value::Object(a)).unwrap();
        let canon_b = to_string(&Value::Object(b)).unwrap();
        assert_eq!(canon_a, canon_b);
        assert_eq!(canon_a, r#"{"first":false,"second":true}"#);
    }
}
