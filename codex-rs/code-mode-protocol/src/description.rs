//! Composes Code Mode descriptions, retaining runtime-owned tool declarations.
//! Exec templates substitute only the documented literal placeholders.

use codex_protocol::ToolName;
use codex_protocol::openai_models::CodeModeToolMessages;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

use crate::PUBLIC_TOOL_NAME;
use crate::json_schema_types::render_compact_input_type;
use crate::json_schema_types::render_json_schema_to_typescript;
use crate::json_schema_types::render_json_schema_to_typescript_with_budget;

const MAX_JS_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Additional tools are callable through tools. Tool availability can change between calls";
pub const TOOL_SEARCH_GUIDANCE: &str = "Use await tools.tool_search({query: \"...\", limit: 8}) for ranked deferred-tool discovery (default limit 8). Print results with text(); inspect declarations before calling tools[result.name](args) in a later execution";
const LEGACY_IMAGE_HELPER_DESCRIPTION: &str = r#"image(dataUrl | { image_url, detail? } | ImageContent, detail?): emit image. Detail: auto/low/high/original. Second detail overrides embedded detail, including MCP _meta["codex/imageDetail"]"#;
const UNIFIED_IMAGE_HELPER_DESCRIPTION: &str =
    "image(dataUrl | { image_url } | ImageContent): emit image";
const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Fresh restricted JavaScript. Source only, no JSON or fences. No console, imports, filesystem, network, Node or browser APIs
Await work. Module end discards unawaited promises. Timers alone cannot keep the isolate alive
Optional first line // @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}; defaults {{ default_exec_yield_time_ms }} ms/10000 tokens
await tools.<normalized_name>(args), or (input) for string tools
Filter ALL_TOOLS by name or description. Print names only. Read selected entries' description for help and schemas
Model-only results bypass JS, return delivery receipts
Helpers:
- text(value): emit text, JSON-stringifying non-strings; bare values are discarded
- {{ image_helper }}
- audio(dataUrl | { audio_url } | AudioContent): emit audio
- generatedImage({ image_url, output_hint? }): emit generated image; data URL only, no HTTP(S)
- store(key, value), load(key): session-persistent serializable values; missing keys return undefined
- notify(value): emit immediate output; yield_control(): yield output while work continues
- exit(): finish successfully
- setTimeout(callback, delayMs?), clearTimeout(id?): schedule/cancel timers
Forward individual MCP content blocks to image/audio, not whole results"#;
const WAIT_DESCRIPTION_TEMPLATE: &str = "Resume or terminate a yielded exec cell";
// Based off of https://modelcontextprotocol.io/specification/draft/schema#calltoolresult
const MCP_TYPESCRIPT_PREAMBLE: &str = r#"type Role = "user" | "assistant";
type MetaObject = Record<string, unknown>;
type Annotations = {
  audience?: Role[];
  priority?: number;
  lastModified?: string;
};
type Icon = {
  src: string;
  mimeType?: string;
  sizes?: string[];
  theme?: "light" | "dark";
};
type TextResourceContents = {
  uri: string;
  mimeType?: string;
  _meta?: MetaObject;
  text: string;
};
type BlobResourceContents = {
  uri: string;
  mimeType?: string;
  _meta?: MetaObject;
  blob: string;
};
type TextContent = {
  type: "text";
  text: string;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type ImageContent = {
  type: "image";
  data: string;
  mimeType: string;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type AudioContent = {
  type: "audio";
  data: string;
  mimeType: string;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type ResourceLink = {
  icons?: Icon[];
  name: string;
  title?: string;
  uri: string;
  description?: string;
  mimeType?: string;
  annotations?: Annotations;
  size?: number;
  _meta?: MetaObject;
  type: "resource_link";
};
type EmbeddedResource = {
  type: "resource";
  resource: TextResourceContents | BlobResourceContents;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type ContentBlock =
  | TextContent
  | ImageContent
  | AudioContent
  | ResourceLink
  | EmbeddedResource;
type CallToolResult<TStructured = { [key: string]: unknown }> = {
  _meta?: MetaObject;
  content: ContentBlock[];
  isError?: boolean;
  structuredContent?: TStructured;
  [key: string]: unknown;
};"#;

pub const CODE_MODE_PRAGMA_PREFIX: &str = "// @exec:";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeToolKind {
    Function,
    Freeform,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub tool_name: ToolName,
    pub description: String,
    pub kind: CodeModeToolKind,
    pub input_schema: Option<JsonValue>,
    /// Internal budget used while rendering the input declaration, before sending it to the host.
    #[serde(skip)]
    pub input_schema_max_bytes: Option<usize>,
    pub output_schema: Option<JsonValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNamespaceDescription {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodeModeExecPragma {
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedExecSource {
    pub code: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, optionally with first-line `// @exec: {\"yield_time_ms\": 10000, \"max_output_tokens\": 1000}`.".to_string(),
        );
    }

    let mut args = ParsedExecSource {
        code: input.to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    };

    let mut lines = input.splitn(2, '\n');
    let first_line = lines.next().unwrap_or_default();
    let rest = lines.next().unwrap_or_default();
    let trimmed = first_line.trim_start();
    let Some(pragma) = trimmed.strip_prefix(CODE_MODE_PRAGMA_PREFIX) else {
        return Ok(args);
    };

    if rest.trim().is_empty() {
        return Err(
            "exec pragma must be followed by JavaScript source on subsequent lines".to_string(),
        );
    }

    let directive = pragma.trim();
    if directive.is_empty() {
        return Err(
            "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
                .to_string(),
        );
    }

    let value: serde_json::Value = serde_json::from_str(directive).map_err(|err| {
        format!(
            "exec pragma must be valid JSON with supported fields `yield_time_ms` and `max_output_tokens`: {err}"
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
            .to_string()
    })?;
    for key in object.keys() {
        match key.as_str() {
            "yield_time_ms" | "max_output_tokens" => {}
            _ => {
                return Err(format!(
                    "exec pragma only supports `yield_time_ms` and `max_output_tokens`; got `{key}`"
                ));
            }
        }
    }

    let pragma: CodeModeExecPragma = serde_json::from_value(value).map_err(|err| {
        format!(
            "exec pragma fields `yield_time_ms` and `max_output_tokens` must be non-negative safe integers: {err}"
        )
    })?;
    if pragma
        .yield_time_ms
        .is_some_and(|yield_time_ms| yield_time_ms > MAX_JS_SAFE_INTEGER)
    {
        return Err(
            "exec pragma field `yield_time_ms` must be a non-negative safe integer".to_string(),
        );
    }
    if pragma.max_output_tokens.is_some_and(|max_output_tokens| {
        u64::try_from(max_output_tokens)
            .map(|max_output_tokens| max_output_tokens > MAX_JS_SAFE_INTEGER)
            .unwrap_or(true)
    }) {
        return Err(
            "exec pragma field `max_output_tokens` must be a non-negative safe integer".to_string(),
        );
    }

    args.code = rest.to_string();
    args.yield_time_ms = pragma.yield_time_ms;
    args.max_output_tokens = pragma.max_output_tokens;
    Ok(args)
}

pub fn is_code_mode_nested_tool(tool_name: &str) -> bool {
    tool_name != crate::PUBLIC_TOOL_NAME && tool_name != crate::WAIT_TOOL_NAME
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageDetailVisibility {
    Visible,
    Hidden,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferredToolDiscovery {
    Catalog,
    RankedSearch,
}

#[allow(clippy::too_many_arguments)]
pub fn build_exec_tool_description(
    enabled_tools: &[ToolDefinition],
    _deferred_tools: &[ToolDefinition],
    namespace_descriptions: &BTreeMap<String, ToolNamespaceDescription>,
    default_exec_yield_time_ms: u64,
    code_mode_only: bool,
    image_detail_visibility: ImageDetailVisibility,
    deferred_tool_discovery: DeferredToolDiscovery,
    messages: Option<&CodeModeToolMessages>,
) -> String {
    let mut sections = Vec::new();
    let image_helper = match image_detail_visibility {
        ImageDetailVisibility::Visible => LEGACY_IMAGE_HELPER_DESCRIPTION,
        ImageDetailVisibility::Hidden => UNIFIED_IMAGE_HELPER_DESCRIPTION,
    };
    let description = messages
        .and_then(|messages| messages.exec.as_ref())
        .and_then(|exec| exec.description.as_deref())
        .unwrap_or(EXEC_DESCRIPTION_TEMPLATE)
        .replace(
            "{{ default_exec_yield_time_ms }}",
            &default_exec_yield_time_ms.to_string(),
        )
        .replace("{{ image_helper }}", image_helper);
    if !description.is_empty() {
        sections.push(description);
    }
    let default_guidance = match deferred_tool_discovery {
        // Standing signatures omit tool prose; keep ranked discovery callable in both modes.
        DeferredToolDiscovery::RankedSearch => {
            format!("{DEFERRED_NESTED_TOOLS_GUIDANCE}\n\n{TOOL_SEARCH_GUIDANCE}")
        }
        DeferredToolDiscovery::Catalog => DEFERRED_NESTED_TOOLS_GUIDANCE.to_string(),
    };
    let guidance = messages
        .and_then(|messages| messages.deferred_nested_tools_guidance.as_deref())
        .unwrap_or(&default_guidance);
    if !guidance.is_empty() {
        sections.push(guidance.to_string());
    }
    if !code_mode_only {
        return sections.join("\n\n");
    }

    // Explicit catalog overrides remain stable across tool changes. The default
    // MCP manual belongs in on-demand tool help, not standing exec descriptions.
    let preamble = messages
        .and_then(|messages| messages.mcp_typescript_preamble.as_deref())
        .unwrap_or_default();
    if !preamble.is_empty() {
        sections.push(format!("Shared MCP Types:\n```ts\n{preamble}\n```"));
    }

    if !enabled_tools.is_empty() {
        let mut current_namespace: Option<&str> = None;
        let mut nested_tool_sections = Vec::with_capacity(enabled_tools.len());

        for tool in enabled_tools {
            let name = tool.name.as_str();
            let namespace_description = tool
                .tool_name
                .namespace
                .as_ref()
                .and_then(|namespace| namespace_descriptions.get(namespace));
            let next_namespace = namespace_description
                .map(|namespace_description| namespace_description.name.as_str());
            if next_namespace != current_namespace {
                if let Some(namespace_description) = namespace_description {
                    let namespace_description_text = namespace_description.description.trim();
                    if !namespace_description_text.is_empty() {
                        nested_tool_sections.push(format!(
                            "## {}\n{namespace_description_text}",
                            namespace_description.name
                        ));
                    }
                }
                current_namespace = next_namespace;
            }

            let input_name = match tool.kind {
                CodeModeToolKind::Function => "args",
                CodeModeToolKind::Freeform => "input",
            };
            let input_type = match tool.kind {
                CodeModeToolKind::Function => tool
                    .input_schema
                    .as_ref()
                    .map(render_compact_input_type)
                    .unwrap_or_else(|| "unknown".to_string()),
                CodeModeToolKind::Freeform => "string".to_string(),
            };
            nested_tool_sections.push(format!(
                "- tools.{}({input_name}: {input_type})",
                normalize_code_mode_identifier(name),
            ));
        }

        sections.push(format!(
            "Tools available in exec:\n{}",
            nested_tool_sections.join("\n")
        ));
    }

    sections.join("\n\n")
}

pub fn build_wait_tool_description() -> &'static str {
    WAIT_DESCRIPTION_TEMPLATE
}

pub fn normalize_code_mode_identifier(tool_key: &str) -> String {
    let mut identifier = String::new();

    for (index, ch) in tool_key.chars().enumerate() {
        let is_valid = if index == 0 {
            ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
        } else {
            ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
        };

        if is_valid {
            identifier.push(ch);
        } else {
            identifier.push('_');
        }
    }

    if identifier.is_empty() {
        "_".to_string()
    } else {
        identifier
    }
}

pub fn augment_tool_definition(mut definition: ToolDefinition) -> ToolDefinition {
    if definition.name != PUBLIC_TOOL_NAME {
        definition.description = render_code_mode_sample_for_definition(&definition);
    }
    definition
}

pub fn enabled_tool_metadata(definition: &ToolDefinition) -> EnabledToolMetadata {
    EnabledToolMetadata {
        tool_name: definition.tool_name.clone(),
        global_name: normalize_code_mode_identifier(&definition.name),
        description: definition.description.clone(),
        kind: definition.kind,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnabledToolMetadata {
    pub tool_name: ToolName,
    pub global_name: String,
    pub description: String,
    pub kind: CodeModeToolKind,
}

pub fn render_code_mode_sample(
    description: &str,
    tool_name: &str,
    input_name: &str,
    input_type: String,
    output_type: String,
) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_code_mode_tool_declaration(tool_name, input_name, &input_type, &output_type)
    );
    format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```")
}

fn render_code_mode_sample_for_definition(definition: &ToolDefinition) -> String {
    let input_name = match definition.kind {
        CodeModeToolKind::Function => "args",
        CodeModeToolKind::Freeform => "input",
    };
    let input_type = match definition.kind {
        CodeModeToolKind::Function => definition
            .input_schema
            .as_ref()
            .map(|schema| match definition.input_schema_max_bytes {
                Some(max_bytes) => render_json_schema_to_typescript_with_budget(schema, max_bytes),
                None => render_json_schema_to_typescript(schema),
            })
            .unwrap_or_else(|| "unknown".to_string()),
        CodeModeToolKind::Freeform => "string".to_string(),
    };
    let output_type = if let Some(structured_content_schema) =
        mcp_structured_content_schema(definition.output_schema.as_ref())
    {
        let structured_content_type = render_json_schema_to_typescript(structured_content_schema);
        if structured_content_type == "unknown" {
            "CallToolResult".to_string()
        } else {
            format!("CallToolResult<{structured_content_type}>")
        }
    } else {
        definition
            .output_schema
            .as_ref()
            .map(render_json_schema_to_typescript)
            .unwrap_or_else(|| "unknown".to_string())
    };
    let mut sample = render_code_mode_sample(
        &definition.description,
        &definition.name,
        input_name,
        input_type,
        output_type,
    );
    // V8 only receives the description string. Keep lossless schemas here too:
    // rendered declarations can be budgeted or approximate unsupported keywords.
    for (label, schema) in [
        ("Input schema", definition.input_schema.as_ref()),
        ("Output schema", definition.output_schema.as_ref()),
    ] {
        if let Some(schema) = schema {
            sample.push_str(&format!("\n\n{label}: {schema}"));
        }
    }
    if mcp_structured_content_schema(definition.output_schema.as_ref()).is_some() {
        format!("{sample}\n\nShared MCP Types:\n```ts\n{MCP_TYPESCRIPT_PREAMBLE}\n```")
    } else {
        sample
    }
}

fn render_code_mode_tool_declaration(
    tool_name: &str,
    input_name: &str,
    input_type: &str,
    output_type: &str,
) -> String {
    let tool_name = normalize_code_mode_identifier(tool_name);
    format!("{tool_name}({input_name}: {input_type}): Promise<{output_type}>;")
}

/// Recognize an MCP result wrapper and return its structured-content schema.
pub fn mcp_structured_content_schema(output_schema: Option<&JsonValue>) -> Option<&JsonValue> {
    let output_schema = output_schema?;
    let properties = output_schema
        .get("properties")
        .and_then(JsonValue::as_object)?;
    let content_schema = properties.get("content").and_then(JsonValue::as_object)?;
    if content_schema.get("type").and_then(JsonValue::as_str) != Some("array") {
        return None;
    }

    if content_schema
        .get("items")
        .and_then(JsonValue::as_object)
        .is_none_or(|items| items.get("type").and_then(JsonValue::as_str) != Some("object"))
    {
        return None;
    }

    if properties
        .get("isError")
        .and_then(JsonValue::as_object)
        .is_none_or(|schema| schema.get("type").and_then(JsonValue::as_str) != Some("boolean"))
    {
        return None;
    }

    if properties
        .get("_meta")
        .and_then(JsonValue::as_object)
        .is_none_or(|schema| schema.get("type").and_then(JsonValue::as_str) != Some("object"))
    {
        return None;
    }

    Some(
        properties
            .get("structuredContent")
            .unwrap_or(&JsonValue::Bool(true)),
    )
}

#[cfg(test)]
#[path = "description_override_tests.rs"]
mod description_override_tests;

#[cfg(test)]
#[path = "description_contract_tests.rs"]
mod description_contract_tests;

#[cfg(test)]
mod tests {
    use super::CodeModeToolKind;
    use super::DeferredToolDiscovery;
    use super::ImageDetailVisibility;
    use super::ParsedExecSource;
    use super::ToolDefinition;
    use super::ToolNamespaceDescription;
    use super::augment_tool_definition;
    use super::build_exec_tool_description;
    use super::normalize_code_mode_identifier;
    use super::parse_exec_source;
    use codex_protocol::ToolName;
    use pretty_assertions::assert_eq;
    use serde_json::Value as JsonValue;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn mcp_call_tool_result_schema(structured_content_schema: JsonValue) -> JsonValue {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "array",
                    "items": {
                        "type": "object"
                    }
                },
                "structuredContent": structured_content_schema,
                "isError": { "type": "boolean" },
                "_meta": { "type": "object" }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    #[test]
    fn parse_exec_source_without_pragma() {
        assert_eq!(
            parse_exec_source("text('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn parse_exec_source_with_pragma() {
        assert_eq!(
            parse_exec_source("// @exec: {\"yield_time_ms\": 10}\ntext('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')".to_string(),
                yield_time_ms: Some(10),
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn normalize_identifier_rewrites_invalid_characters() {
        assert_eq!(
            "mcp__ologs__get_profile",
            normalize_code_mode_identifier("mcp__ologs__get_profile")
        );
        assert_eq!(
            "hidden_dynamic_tool",
            normalize_code_mode_identifier("hidden-dynamic-tool")
        );
    }

    #[test]
    fn augment_tool_definition_appends_typed_declaration() {
        let definition = ToolDefinition {
            name: "hidden_dynamic_tool".to_string(),
            tool_name: ToolName::plain("hidden_dynamic_tool"),
            description: "Test tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
                "additionalProperties": false
            })),
            input_schema_max_bytes: None,
            output_schema: Some(json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains("declare const tools"));
        assert!(
            description.contains(
                "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;"
            )
        );
    }

    #[test]
    fn augment_tool_definition_includes_property_descriptions_as_comments() {
        let definition = ToolDefinition {
            name: "weather_tool".to_string(),
            tool_name: ToolName::plain("weather_tool"),
            description: "Weather tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "weather": {
                        "type": "array",
                        "description": "look up weather for a given list of locations",
                        "items": {
                            "type": "object",
                            "properties": {
                                "location": { "type": "string" }
                            },
                            "required": ["location"]
                        }
                    }
                },
                "required": ["weather"]
            })),
            input_schema_max_bytes: None,
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "forecast": {
                        "type": "string",
                        "description": "human readable weather forecast"
                    }
                },
                "required": ["forecast"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains(
            r#"weather_tool(args: {
  // look up weather for a given list of locations
  weather: Array<{ location: string; }>;
}): Promise<{
  // human readable weather forecast
  forecast: string;
}>;"#
        ));
    }

    #[test]
    fn code_mode_types_structured_content_result_refs() {
        let definition = ToolDefinition {
            name: "mcp__sample__search".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "search"),
            description: "Search".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            input_schema_max_bytes: None,
            output_schema: Some(mcp_call_tool_result_schema(json!({
                "type": "object",
                "properties": {
                    "results": {
                        "type": "array",
                        "items": { "$ref": "#/definitions/Result~1item~0v1" }
                    }
                },
                "required": ["results"],
                "additionalProperties": false,
                "definitions": {
                    "Result/item~v1": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "score": { "type": "number" }
                        },
                        "required": ["id", "score"],
                        "additionalProperties": false
                    }
                }
            }))),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains(
            "mcp__sample__search(args: {}): Promise<CallToolResult<{ results: Array<{ id: string; score: number; }>; }>>;"
        ));
    }

    #[test]
    fn code_mode_only_description_includes_nested_tools() {
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "foo".to_string(),
                tool_name: ToolName::plain("foo"),
                description: "NESTED_TOOL_PROSE_MUST_STAY_ON_DEMAND".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                input_schema_max_bytes: None,
                output_schema: None,
            }],
            &[],
            &BTreeMap::new(),
            crate::DEFAULT_EXEC_YIELD_TIME_MS,
            /*code_mode_only*/ true,
            ImageDetailVisibility::Visible,
            DeferredToolDiscovery::Catalog,
            /*messages*/ None,
        );
        assert!(description.contains("- tools.foo(args: unknown)"));
        assert!(!description.contains("NESTED_TOOL_PROSE_MUST_STAY_ON_DEMAND"));
        assert!(!description.contains("do not attempt to use any other tools directly"));
    }

    #[test]
    fn exec_description_mentions_timeout_helpers() {
        let description = build_exec_tool_description(
            &[],
            &[],
            &BTreeMap::new(),
            crate::DEFAULT_EXEC_YIELD_TIME_MS,
            /*code_mode_only*/ false,
            ImageDetailVisibility::Visible,
            DeferredToolDiscovery::Catalog,
            /*messages*/ None,
        );
        assert!(description.contains("audio(dataUrl | { audio_url } | AudioContent)"));
        assert!(description.contains("setTimeout(callback, delayMs?)"));
        assert!(description.contains("clearTimeout(id?)"));
    }

    #[test]
    fn code_mode_only_description_groups_namespace_instructions_once() {
        let namespace_descriptions = BTreeMap::from([(
            "mcp__sample__".to_string(),
            ToolNamespaceDescription {
                name: "mcp__sample".to_string(),
                description: "Shared namespace guidance.".to_string(),
            },
        )]);
        let description = build_exec_tool_description(
            &[
                ToolDefinition {
                    name: "mcp__sample__alpha".to_string(),
                    tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
                    description: "First tool".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })),
                    input_schema_max_bytes: None,
                    output_schema: Some(mcp_call_tool_result_schema(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }))),
                },
                ToolDefinition {
                    name: "mcp__sample__beta".to_string(),
                    tool_name: ToolName::namespaced("mcp__sample__", "beta"),
                    description: "Second tool".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })),
                    input_schema_max_bytes: None,
                    output_schema: Some(mcp_call_tool_result_schema(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }))),
                },
            ],
            &[],
            &namespace_descriptions,
            crate::DEFAULT_EXEC_YIELD_TIME_MS,
            /*code_mode_only*/ true,
            ImageDetailVisibility::Visible,
            DeferredToolDiscovery::Catalog,
            /*messages*/ None,
        );
        assert_eq!(description.matches("## mcp__sample").count(), 1);
        assert!(description.contains("## mcp__sample\nShared namespace guidance."));
        assert!(description.contains("- tools.mcp__sample__alpha(args: {})"));
        assert!(description.contains("- tools.mcp__sample__beta(args: {})"));
    }

    #[test]
    fn code_mode_only_description_omits_empty_namespace_sections() {
        let namespace_descriptions = BTreeMap::from([(
            "mcp__sample__".to_string(),
            ToolNamespaceDescription {
                name: "mcp__sample".to_string(),
                description: String::new(),
            },
        )]);
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "mcp__sample__alpha".to_string(),
                tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
                description: "First tool".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })),
                input_schema_max_bytes: None,
                output_schema: Some(mcp_call_tool_result_schema(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }))),
            }],
            &[],
            &namespace_descriptions,
            crate::DEFAULT_EXEC_YIELD_TIME_MS,
            /*code_mode_only*/ true,
            ImageDetailVisibility::Visible,
            DeferredToolDiscovery::Catalog,
            /*messages*/ None,
        );

        assert!(!description.contains("## mcp__sample"));
        assert!(description.contains("- tools.mcp__sample__alpha(args: {})"));
    }

    #[test]
    fn code_mode_only_description_keeps_default_mcp_types_on_demand() {
        let first_tool = augment_tool_definition(ToolDefinition {
            name: "mcp__sample__alpha".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
            description: "First tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            input_schema_max_bytes: None,
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        }
                    },
                    "structuredContent": {
                        "type": "object",
                        "properties": {
                            "echo": { "type": "string" }
                        },
                        "required": ["echo"],
                        "additionalProperties": false
                    },
                    "isError": { "type": "boolean" },
                    "_meta": { "type": "object" }
                },
                "required": ["content"],
                "additionalProperties": false
            })),
        });
        let second_tool = augment_tool_definition(ToolDefinition {
            name: "mcp__sample__beta".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "beta"),
            description: "Second tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            input_schema_max_bytes: None,
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        }
                    },
                    "structuredContent": {
                        "type": "object",
                        "properties": {
                            "count": { "type": "integer" }
                        },
                        "required": ["count"],
                        "additionalProperties": false
                    },
                    "isError": { "type": "boolean" },
                    "_meta": { "type": "object" }
                },
                "required": ["content"],
                "additionalProperties": false
            })),
        });

        let description = build_exec_tool_description(
            &[
                ToolDefinition {
                    name: first_tool.name,
                    tool_name: first_tool.tool_name,
                    description: "First tool".to_string(),
                    kind: first_tool.kind,
                    input_schema: first_tool.input_schema,
                    input_schema_max_bytes: None,
                    output_schema: first_tool.output_schema,
                },
                ToolDefinition {
                    name: second_tool.name,
                    tool_name: second_tool.tool_name,
                    description: "Second tool".to_string(),
                    kind: second_tool.kind,
                    input_schema: second_tool.input_schema,
                    input_schema_max_bytes: None,
                    output_schema: second_tool.output_schema,
                },
            ],
            &[],
            &BTreeMap::new(),
            crate::DEFAULT_EXEC_YIELD_TIME_MS,
            /*code_mode_only*/ true,
            ImageDetailVisibility::Visible,
            DeferredToolDiscovery::Catalog,
            /*messages*/ None,
        );

        assert_eq!(
            description
                .matches("type CallToolResult<TStructured = { [key: string]: unknown }>")
                .count(),
            0
        );
        assert_eq!(description.matches("Shared MCP Types:").count(), 0);
    }

    #[test]
    fn code_mode_only_description_omits_default_mcp_types_for_deferred_tools() {
        let deferred_tool = ToolDefinition {
            name: "mcp__sample__alpha".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
            description: "Deferred tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            input_schema_max_bytes: None,
            output_schema: Some(mcp_call_tool_result_schema(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }))),
        };

        let description = build_exec_tool_description(
            &[],
            &[deferred_tool],
            &BTreeMap::new(),
            crate::DEFAULT_EXEC_YIELD_TIME_MS,
            /*code_mode_only*/ true,
            ImageDetailVisibility::Visible,
            DeferredToolDiscovery::Catalog,
            /*messages*/ None,
        );

        assert!(description.contains(super::DEFERRED_NESTED_TOOLS_GUIDANCE));
        assert!(!description.contains("Shared MCP Types:"));
        assert!(!description.contains("### `mcp__sample__alpha`"));
    }

    #[test]
    fn exec_description_preserves_discovery_guidance_across_catalog_changes() {
        let deferred_tools = [ToolDefinition {
            name: "deferred_tool".to_string(),
            tool_name: ToolName::plain("deferred_tool"),
            description: "Deferred tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            input_schema_max_bytes: None,
            output_schema: None,
        }];
        let search_tool = ToolDefinition {
            name: "tool_search".to_string(),
            tool_name: ToolName::plain("tool_search"),
            description: super::TOOL_SEARCH_GUIDANCE.to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            input_schema_max_bytes: None,
            output_schema: None,
        };
        for code_mode_only in [false, true] {
            for discovery in [
                DeferredToolDiscovery::Catalog,
                DeferredToolDiscovery::RankedSearch,
            ] {
                let enabled_tools = match discovery {
                    DeferredToolDiscovery::Catalog => Vec::new(),
                    DeferredToolDiscovery::RankedSearch => vec![search_tool.clone()],
                };
                let description = build_exec_tool_description(
                    &enabled_tools,
                    &deferred_tools,
                    &BTreeMap::new(),
                    crate::DEFAULT_EXEC_YIELD_TIME_MS,
                    code_mode_only,
                    ImageDetailVisibility::Visible,
                    discovery,
                    /*messages*/ None,
                );
                assert!(description.contains(super::DEFERRED_NESTED_TOOLS_GUIDANCE));
                assert!(description.contains("Filter ALL_TOOLS by name or description"));
                assert_eq!(
                    description.matches("await tools.tool_search(").count(),
                    usize::from(discovery == DeferredToolDiscovery::RankedSearch),
                );

                let empty_catalog_description = build_exec_tool_description(
                    &enabled_tools,
                    &[],
                    &BTreeMap::new(),
                    crate::DEFAULT_EXEC_YIELD_TIME_MS,
                    code_mode_only,
                    ImageDetailVisibility::Visible,
                    discovery,
                    /*messages*/ None,
                );
                assert_eq!(description, empty_catalog_description);
            }
        }
    }
}
