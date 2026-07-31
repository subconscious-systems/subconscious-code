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
///
/// **Prefer [`to_bytes`] for request bodies.** This routes through a
/// `serde_json::Value` tree, which costs several full copies of the payload —
/// fine for a tool schema, ruinous for a multi-hundred-megabyte conversation.
pub fn to_string<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let v = serde_json::to_value(value)?;
    Ok(canonicalize(&v).to_string())
}

/// Serialize `value` straight to bytes, with no intermediate `Value` tree.
///
/// This is the request-body path, and the reason it exists is memory. The
/// [`to_string`] route materializes the whole payload three times over — a
/// `Value` tree (itself several times the size of the JSON it represents), a
/// canonicalized copy of that tree, then the output `String` — before `reqwest`
/// gets a byte of it. This function makes exactly one pass into one buffer,
/// which `Bytes` then adopts without copying.
///
/// Measured end-to-end (a 12 MB tool result through the full assembly path):
/// 86.7 MB peak RSS against a 15.2 MB baseline, i.e. ~6× the payload. The
/// remaining multiple is *not* here — it's the `Turn`/`WireMessage` clones in
/// the assembly pipeline (`rc_ctx::prepare_turns` → `project_with` → this
/// function). Serialization is one copy of that total now instead of four.
///
/// The output is still byte-stable across calls, which is what §4.6 actually
/// requires for prefix caching:
///
/// - `serde` emits struct fields in declaration order — fixed at compile time.
/// - `serde_json::Map` is a `BTreeMap` (we do not enable `preserve_order`), so
///   any dynamic map — tool schemas, tool-call arguments — serializes with keys
///   sorted regardless of insertion order.
///
/// Both properties are pinned by tests below. What this does *not* do is sort
/// struct fields alphabetically the way [`to_string`] did; the bytes differ from
/// the old canonical form, but they are equally stable, which is the property
/// prefix caching depends on.
pub fn to_bytes<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(value)
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

    /// The property the request-body path relies on: serializing the same value
    /// twice yields identical bytes, so a retry re-sends the same prefix.
    #[test]
    fn to_bytes_is_stable_across_calls() {
        let s = Schema { z: 1, a: 2, m: Nested { y: 3, b: 4 } };
        assert_eq!(to_bytes(&s).unwrap(), to_bytes(&s).unwrap());
    }

    /// Struct fields come out in declaration order — deterministic, though not
    /// alphabetized the way `to_string` canonicalizes.
    #[test]
    fn to_bytes_uses_declaration_order() {
        let s = Schema { z: 1, a: 2, m: Nested { y: 3, b: 4 } };
        let out = String::from_utf8(to_bytes(&s).unwrap()).unwrap();
        assert_eq!(out, r#"{"z":1,"a":2,"m":{"y":3,"b":4}}"#);
    }

    /// Dynamic maps (tool schemas, tool-call arguments) still serialize with
    /// sorted keys because `serde_json::Map` is a `BTreeMap`. This test fails
    /// loudly if anyone enables serde_json's `preserve_order` feature, which
    /// would make insertion order leak into the wire bytes and silently destroy
    /// prefix-cache hit rates.
    #[test]
    fn to_bytes_sorts_dynamic_map_keys() {
        let mut a = serde_json::Map::new();
        a.insert("second".to_string(), Value::Bool(true));
        a.insert("first".to_string(), Value::Bool(false));

        let mut b = serde_json::Map::new();
        b.insert("first".to_string(), Value::Bool(false));
        b.insert("second".to_string(), Value::Bool(true));

        let ba = to_bytes(&Value::Object(a)).unwrap();
        let bb = to_bytes(&Value::Object(b)).unwrap();
        assert_eq!(ba, bb, "preserve_order must stay off — see the doc comment");
        assert_eq!(String::from_utf8(ba).unwrap(), r#"{"first":false,"second":true}"#);
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
