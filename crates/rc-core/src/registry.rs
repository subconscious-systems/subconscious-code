//! The tool registry: the registered tool set + cached wire definitions (§4.6).
//!
//! Built once at session start; the on-wire `tools` bytes are stable across the
//! turn. (MCP servers connecting late or `/agents` toggling a tool invalidate
//! the prefix — M9 batches MCP connection before the first request.)

use crate::tool::Tool;
use rc_proto::{FunctionDefinition, ToolDefinition};
use std::sync::Arc;

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    defs: Vec<ToolDefinition>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let defs = tools
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
        Self { tools, defs }
    }

    /// The wire tool definitions, ready for the request's `tools` array.
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.defs
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }
}
