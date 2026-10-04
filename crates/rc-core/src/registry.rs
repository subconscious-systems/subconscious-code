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

/// A duplicate tool name in a [`ToolRegistry`] build. Two tools answering the
/// same name silently first-wins on `get` while both appear (differently
/// described) to the model — the registry refuses to build instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateToolNameError {
    pub name: String,
}

impl std::fmt::Display for DuplicateToolNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "duplicate tool name: {}", self.name)
    }
}

impl std::error::Error for DuplicateToolNameError {}

impl ToolRegistry {
    /// Checked build: detect duplicate tool names before the registry is used.
    ///
    /// Duplicate names previously resolved silently first-wins for `get` —
    /// while the wire `tools` array exposed both definitions — which is
    /// exactly the state a caller can't diagnose later. This returns an error
    /// naming the collision instead.
    pub fn try_new(
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<Self, DuplicateToolNameError> {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for tool in &tools {
            if !seen.insert(tool.name()) {
                return Err(DuplicateToolNameError {
                    name: tool.name().to_string(),
                });
            }
        }
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

        Ok(Self { tools, defs })
    }

    /// Build a registry, **panicking on duplicate tool names** (see
    /// [`Self::try_new`]). Kept infallible for the fixed build-time tool sets
    /// the composition roots register; callers assembling tools dynamically
    /// (MCP servers, plugins) should use [`Self::try_new`] and surface the
    /// error instead of aborting.
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        match Self::try_new(tools) {
            Ok(registry) => registry,
            Err(error) => panic!("{error}: refusing to silently shadow a tool"),
        }
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
    use crate::tool::{Tool, ToolCtx, ToolError, ToolOutcome};
    use async_trait::async_trait;
    use rc_proto::wire::{FunctionDefinition, ToolDefinition, ToolType};
    use serde_json::{json, Value};

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

    /// The smallest possible `Tool` stub: name, empty schema, no-op call.
    struct Stub(&'static str);

    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("stub".into()))
        }
    }

    /// A duplicate name must fail the checked build instead of silently
    /// first-wins on `get` (with both descriptions on the wire).
    #[test]
    fn try_new_reports_duplicate_tool_names() {
        let error = match ToolRegistry::try_new(vec![
            Arc::new(Stub("Echo")) as Arc<dyn Tool>,
            Arc::new(Stub("Echo")),
        ]) {
            Ok(_) => panic!("two tools named Echo must not build"),
            Err(error) => error,
        };
        assert_eq!(error.name, "Echo");
        assert!(error.to_string().contains("duplicate tool name"));
    }

    #[test]
    fn try_new_accepts_a_distinct_tool_set() {
        let registry =
            ToolRegistry::try_new(vec![Arc::new(Stub("Echo")) as Arc<dyn Tool>]).unwrap();
        assert!(registry.get("Echo").is_some());
        assert_eq!(registry.definitions().len(), 1);
    }

    /// The infallible constructor enforces the same invariant at build time —
    /// a hardcoded composition root with a duplicate is a programming error.
    #[test]
    #[should_panic(expected = "duplicate tool name: Echo")]
    fn new_panics_on_duplicate_tool_names() {
        let _ = ToolRegistry::new(vec![
            Arc::new(Stub("Echo")) as Arc<dyn Tool>,
            Arc::new(Stub("Echo")),
        ]);
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
