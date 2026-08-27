//! The tool registry: the registered tool set + cached wire definitions (§4.6).
//!
//! Built once at session start; the on-wire `tools` bytes are stable across the
//! turn *and across sessions*: the `tools` array is the orbit-canonical
//! representative of the registered tool set (stable sort by content hash), so
//! two sessions that register the same tools in different orders — including
//! nondeterministic MCP connect order — emit identical `tools` bytes and thus
//! the same prefix-cache key. Without this, a reordered `tools` array diverges
//! from the first byte and zero-s the cache hit rate against a prefix-caching
//! router. (MCP servers connecting late or `/agents` toggling a tool still
//! invalidate the prefix by changing the *set* — M9 batches MCP connection
//! before the first request.)

use crate::tool::Tool;
use rc_algebra::multiset::BlockId;
use rc_algebra::orbit::{canonical_representative, orbit_divergence};
use rc_proto::canonical;
use rc_proto::{FunctionDefinition, ToolDefinition};
use std::sync::Arc;

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    defs: Vec<ToolDefinition>,
}

/// The content-hash key of a tool definition: the SHA-256 of its canonical
/// (sorted-key, compact) serialized bytes. Two definitions with the same name
/// and schema hash equal regardless of construction order.
fn def_key(d: &ToolDefinition) -> BlockId {
    match canonical::to_bytes(d) {
        Ok(bytes) => BlockId::from_bytes(&bytes),
        // A `ToolDefinition` is plain JSON and should always serialize; if it
        // ever doesn't, fall back to the function name so ordering is still
        // stable rather than panicking at session start.
        Err(_) => BlockId::from_bytes(d.function.name.as_bytes()),
    }
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut defs: Vec<ToolDefinition> = tools
            .iter()
            .map(|t| ToolDefinition {
                ty: Default::default(),
                function: FunctionDefinition {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.schema(),
                },
            })
            .collect();

        // Orbit canonicalization (§4.6): collapse the S_n orbit of tool
        // definitions onto one representative so registration order doesn't
        // leak into the wire bytes. Instrument how often the raw order would
        // have diverged — the tail of a high cache hit rate is often exactly
        // this kind of nondeterministic ordering.
        let divergence = orbit_divergence(&defs, def_key);
        if !divergence.already_canonical {
            tracing::debug!(
                target: "sc.orbit",
                tools = defs.len(),
                "tool-definition order was non-canonical; sorted by content hash \
                 (raw != canonical, would have diverged the prefix)"
            );
        }
        canonical_representative(&mut defs, def_key);

        Self { tools, defs }
    }

    /// The wire tool definitions, ready for the request's `tools` array, in
    /// orbit-canonical (content-hash) order.
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.defs
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::wire::{FunctionDefinition, ToolDefinition, ToolType};

    fn def(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition {
            ty: ToolType::Function,
            function: FunctionDefinition {
                name: name.to_string(),
                description: desc.to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
            },
        }
    }

    /// Two sessions registering the same tool set in different orders must
    /// produce identical on-wire `tools` bytes — the cache-hit-rate regression.
    #[test]
    fn two_registration_orders_produce_identical_bytes() {
        let set_a = vec![def("zebra", "z"), def("alpha", "a"), def("mike", "m")];
        let set_b = vec![def("mike", "m"), def("zebra", "z"), def("alpha", "a")];

        // Simulate `ToolRegistry::new`'s canonicalization directly on defs
        // (the builder path is exercised end-to-end via the live tool set in
        // rc-core/tests; here we pin the byte-equivalence property).
        let mut a = set_a.clone();
        let mut b = set_b.clone();
        canonical_representative(&mut a, def_key);
        canonical_representative(&mut b, def_key);
        assert_eq!(
            canonical::to_bytes(&a).unwrap(),
            canonical::to_bytes(&b).unwrap()
        );
    }

    #[test]
    fn canonical_order_is_stable_sort() {
        let mut defs = vec![def("zebra", "z"), def("alpha", "a"), def("mike", "m")];
        canonical_representative(&mut defs, def_key);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        // Sorted by content hash, not by name — but distinct content gives a
        // total order; just assert it's a permutation and stable across runs.
        let mut again = vec![def("zebra", "z"), def("alpha", "a"), def("mike", "m")];
        canonical_representative(&mut again, def_key);
        let names_again: Vec<&str> = again.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(names, names_again);
    }
}
