//! Standing signatures are lossy hints. Runtime help must remain lossless.

use super::CodeModeToolKind;
use super::DeferredToolDiscovery;
use super::ImageDetailVisibility;
use super::MCP_TYPESCRIPT_PREAMBLE;
use super::ToolDefinition;
use super::ToolNamespaceDescription;
use super::augment_tool_definition;
use super::build_exec_tool_description;
use super::enabled_tool_metadata;
use super::render_code_mode_sample;
use super::render_json_schema_to_typescript;
use codex_protocol::ToolName;
use codex_protocol::openai_models::CodeModeToolMessages;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

fn definition(name: &str, kind: CodeModeToolKind) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        tool_name: ToolName::plain(name),
        description: "Full help that must not become standing prose".to_string(),
        kind,
        input_schema: Some(json!({
            "type": "object",
            "properties": {
                "mode": {"enum": ["fast", "safe"], "description": "Mode help"},
                "queries": {"type": "array", "items": {"$ref": "#/$defs/query"}},
                "options": {"$ref": "#/$defs/query"}
            },
            "required": ["queries"],
            "additionalProperties": false,
            "$defs": {"query": {
                "type": "object",
                "properties": {"q": {"type": "string", "description": "Query help"}},
                "required": ["q"],
                "additionalProperties": false
            }}
        })),
        input_schema_max_bytes: None,
        output_schema: Some(json!({"type": "string", "description": "Result help"})),
    }
}

fn standing(
    tools: &[ToolDefinition],
    namespaces: &BTreeMap<String, ToolNamespaceDescription>,
) -> String {
    build_exec_tool_description(
        tools,
        &[],
        namespaces,
        30000,
        true,
        ImageDetailVisibility::Visible,
        DeferredToolDiscovery::Catalog,
        Some(&CodeModeToolMessages {
            exec: Some(codex_protocol::openai_models::ToolMessage {
                description: Some(String::new()),
                ..Default::default()
            }),
            deferred_nested_tools_guidance: Some(String::new()),
            ..Default::default()
        }),
    )
}

#[test]
fn compact_inventory_preserves_call_shape_without_nested_manuals() {
    let function = definition("hidden-dynamic", CodeModeToolKind::Function);
    let freeform = definition("patch", CodeModeToolKind::Freeform);
    assert_eq!(
        standing(&[function.clone(), freeform.clone()], &BTreeMap::new()),
        "Tools available in exec:\n- tools.hidden_dynamic(args: { mode?: \"fast\" | \"safe\"; options?: object; queries: Array<object>; })\n- tools.patch(input: string)",
    );

    // This is the metadata consumed by V8 ALL_TOOLS, not a separate test facade.
    let full = augment_tool_definition(function.clone());
    let metadata = enabled_tool_metadata(&full);
    assert_eq!(metadata.global_name, "hidden_dynamic");
    assert_eq!(
        metadata.description,
        format!(
            "{}\n\nInput schema: {}\n\nOutput schema: {}",
            render_code_mode_sample(
                &function.description,
                &function.name,
                "args",
                render_json_schema_to_typescript(function.input_schema.as_ref().unwrap()),
                "string".to_string(),
            ),
            function.input_schema.as_ref().unwrap(),
            function.output_schema.as_ref().unwrap(),
        )
    );
    assert_eq!(full.input_schema, function.input_schema);
    assert_eq!(full.output_schema, function.output_schema);
    assert_eq!(
        augment_tool_definition(freeform.clone()).description,
        format!(
            "{}\n\nInput schema: {}\n\nOutput schema: {}",
            render_code_mode_sample(
                &freeform.description,
                &freeform.name,
                "input",
                "string".to_string(),
                "string".to_string()
            ),
            freeform.input_schema.as_ref().unwrap(),
            freeform.output_schema.as_ref().unwrap()
        ),
    );
}

#[test]
fn namespaces_keep_shared_guidance_once_without_restating_tool_help() {
    let namespaces = BTreeMap::from([(
        "group".to_string(),
        ToolNamespaceDescription {
            name: "group".to_string(),
            description: "Shared safety boundary".to_string(),
        },
    )]);
    let mut alpha = definition("group__alpha", CodeModeToolKind::Freeform);
    alpha.tool_name = ToolName::namespaced("group", "alpha");
    let mut beta = alpha.clone();
    beta.name = "group__beta".to_string();
    beta.tool_name = ToolName::namespaced("group", "beta");
    assert_eq!(
        standing(&[alpha, beta], &namespaces),
        "Tools available in exec:\n## group\nShared safety boundary\n- tools.group__alpha(input: string)\n- tools.group__beta(input: string)"
    );
}

#[test]
fn mcp_types_move_to_discoverable_help_with_lossless_schemas() {
    let mut tool = definition("mcp__search", CodeModeToolKind::Function);
    tool.output_schema = Some(json!({"type": "object", "properties": {
        "content": {"type": "array", "items": {"type": "object"}},
        "structuredContent": {"type": "string"},
        "isError": {"type": "boolean"}, "_meta": {"type": "object"}
    }}));
    let ordinary = definition("mcp__search", CodeModeToolKind::Function);
    assert_eq!(
        standing(&[tool.clone()], &BTreeMap::new()),
        standing(&[ordinary], &BTreeMap::new())
    );
    let full = augment_tool_definition(tool.clone());
    assert_eq!(
        enabled_tool_metadata(&full).description,
        format!(
            "{}\n\nInput schema: {}\n\nOutput schema: {}\n\nShared MCP Types:\n```ts\n{MCP_TYPESCRIPT_PREAMBLE}\n```",
            render_code_mode_sample(
                &tool.description,
                &tool.name,
                "args",
                render_json_schema_to_typescript(tool.input_schema.as_ref().unwrap()),
                "CallToolResult<string>".to_string()
            ),
            tool.input_schema.as_ref().unwrap(),
            tool.output_schema.as_ref().unwrap(),
        )
    );
}

#[test]
fn declaration_budget_cannot_drop_discoverable_schema() {
    let mut tool = definition("budgeted", CodeModeToolKind::Function);
    tool.input_schema_max_bytes = Some(1);
    let metadata = enabled_tool_metadata(&augment_tool_definition(tool.clone()));
    assert_eq!(
        metadata.description,
        format!(
            "{}\n\nInput schema: {}\n\nOutput schema: {}",
            render_code_mode_sample(
                &tool.description,
                &tool.name,
                "args",
                "unknown".to_string(),
                "string".to_string()
            ),
            tool.input_schema.as_ref().unwrap(),
            tool.output_schema.as_ref().unwrap(),
        ),
    );
}
