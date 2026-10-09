use std::collections::BTreeMap;

use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolNamespaceDescription;
use codex_code_mode::mcp_structured_content_schema;
use codex_code_mode::normalize_code_mode_identifier;
use codex_code_mode::render_compact_input_type;
use codex_protocol::openai_models::CodeModeToolMessages;
use serde_json::Value;

const NOTEBOOK_USAGE: &str = r#"Persistent Deno/TypeScript notebook: console, imports, Deno and Web APIs. Full machine access, no sandbox
Bindings/imports survive cells and context rollover. Checkpoints restore values without replay, functions from source without closures or live handles. New threads inherit durable project state only
Await work. Cell end cancels unawaited tool calls, not timers or direct I/O. Wait on yielded cells before another exec. Cancellation kills the kernel. Next exec restores its checkpoint without replay
await tools.NAME(args), or tools.NAME(input) for string tools. Model-only results bypass JS, return delivery receipts
generatedImage({image_url, output_hint?}). store(key,value)/load(key): kernel-local JSON values. await notify(value): immediate output. await yield_control(): yield while work continues. exit(): success. No audio()"#;

/// A usage surface only. Tool definitions and their on-demand help still own
/// validation, complete schemas, and normalized identities.
pub(crate) fn build_notebook_tools_prompt(
    enabled_tools: &[ToolDefinition],
    deferred_tools: &[ToolDefinition],
    namespace_descriptions: &BTreeMap<String, ToolNamespaceDescription>,
    nested_notebook_available: bool,
    messages: Option<&CodeModeToolMessages>,
) -> String {
    let mut sections = vec![NOTEBOOK_USAGE.to_string()];
    if nested_notebook_available {
        sections.push("Inside exec: tools.notebook supports status without query, list, diagnostics. Other actions require top-level notebook after exec returns".to_string());
    }
    if !enabled_tools.is_empty() {
        let mut entries = Vec::new();
        let mut current_namespace = None;
        for tool in enabled_tools {
            let namespace = tool
                .tool_name
                .namespace
                .as_ref()
                .and_then(|name| namespace_descriptions.get(name));
            let next_namespace = namespace.map(|namespace| namespace.name.as_str());
            if next_namespace != current_namespace {
                if let Some(namespace) = namespace
                    && !namespace.description.trim().is_empty()
                {
                    entries.push(format!(
                        "## {}\n{}",
                        namespace.name,
                        namespace.description.trim()
                    ));
                }
                current_namespace = next_namespace;
            }
            entries.push(tool_usage(tool));
        }
        sections.push(format!(
            "Tools in exec (selected fields):\n{}",
            entries.join("\n")
        ));
    }
    if !deferred_tools.is_empty() {
        let guidance = messages
            .and_then(|messages| messages.deferred_nested_tools_guidance.as_deref())
            .unwrap_or("Additional tools in ALL_TOOLS");
        if !guidance.is_empty() {
            sections.push(guidance.to_string());
        }
    }
    // Preserve explicit model guidance without loading the bundled MCP manual.
    // Native declarations in on-demand help already carry the full MCP types.
    if let Some(preamble) = messages
        .and_then(|messages| messages.mcp_typescript_preamble.as_deref())
        .filter(|preamble| !preamble.is_empty())
        && enabled_tools
            .iter()
            .chain(deferred_tools)
            .any(|tool| mcp_structured_content_schema(tool.output_schema.as_ref()).is_some())
    {
        sections.push(format!("Shared MCP Types:\n```ts\n{preamble}\n```"));
    }
    sections.push("Filter ALL_TOOLS by name or description. Print names only. Read tools.NAME.description for selected tools.".to_string());
    format!("<exec_tools>\n{}\n</exec_tools>", sections.join("\n\n"))
}

fn tool_usage(tool: &ToolDefinition) -> String {
    let name = normalize_code_mode_identifier(&tool.name);
    // Select by original identity, never by an MCP alias resembling a native tool.
    if tool.tool_name.is_default_namespace() {
        if tool.tool_name.name == "skills" && tool.kind == CodeModeToolKind::Freeform {
            return format!(
                r#"- await tools.{name}("list") // or "list <category>...", "read <skill> [skill-or-reference...]""#
            );
        }
        if tool.tool_name.name == "apply_patch" && tool.kind == CodeModeToolKind::Freeform {
            return format!(
                "- await tools.{name}(patch) // Raw string: *** Begin Patch / *** End Patch. Actions: *** Add File: path (+ lines), *** Update File: path, *** Delete File: path. *** Move to: path immediately after Update File, with a nonempty @@ hunk (one unchanged context line for a pure move). Update hunks: @@, exact context, space/+/- prefixes, file order. @@ text: context, not a line range"
            );
        }
        if tool.tool_name.name == "request_user_input"
            && tool.kind == CodeModeToolKind::Function
            && let Some(delivery) = tool
                .input_schema
                .as_ref()
                .and_then(|schema| schema["properties"].get("delivery"))
        {
            let delivery = render_compact_input_type(delivery);
            return format!("- await tools.{name}(args) // delivery?: {delivery} (default wait)");
        }
        if let Some(fields) = common_fields(&tool.tool_name.name)
            && tool.kind == CodeModeToolKind::Function
            && let Some(schema) = tool.input_schema.as_ref()
        {
            let input = render_compact_input_type(&select_fields(schema, fields));
            let note = match tool.tool_name.name.as_str() {
                "exec_command"
                    if schema["properties"].get("timeout_ms").is_some()
                        && schema["properties"].get("yield_time_ms").is_none() =>
                {
                    "returns {output: string, wall_time_seconds: number, exit_code?: number, truncated?: boolean, ...}. Runs to completion. Timeout or cancellation terminates it"
                }
                "exec_command" => {
                    "returns {output: string, wall_time_seconds: number, session_id?: number, exit_code?: number, truncated?: boolean, ...}; poll a running session with write_stdin"
                }
                "write_stdin" => {
                    "returns command output and continuation/exit fields. Non-empty chars require tty=true in exec_command. Empty/omitted chars poll"
                }
                "view_image" => "returns {image_url, detail?}; pass the result to image(result)",
                "get_context_remaining" => "returns {tokens_left: number | null}",
                _ => "",
            };
            return format!("- await tools.{name}(args: {input}) // {note}");
        }
    }
    let input = match tool.kind {
        CodeModeToolKind::Function => "args",
        CodeModeToolKind::Freeform => "input: string",
    };
    // Augmented descriptions contain full declarations after the capability.
    // Keep those declarations in help, not in this standing inventory.
    let capability = tool
        .description
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    if capability.is_empty() || capability.chars().count() > 160 {
        format!("- tools.{name}({input})")
    } else {
        format!("- tools.{name}({input}) // {capability}")
    }
}

fn common_fields(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "exec_command" => Some(&[
            "cmd",
            "workdir",
            "shell",
            "tty",
            "yield_time_ms",
            "timeout_ms",
            "max_output_tokens",
            "login",
        ]),
        "write_stdin" => Some(&["session_id", "chars", "yield_time_ms", "max_output_tokens"]),
        "view_image" => Some(&["path", "detail"]),
        "get_context_remaining" => Some(&[]),
        _ => None,
    }
}

fn select_fields(schema: &Value, fields: &[&str]) -> Value {
    let mut selected = schema.clone();
    let required = schema.get("required").and_then(Value::as_array);
    if let Some(properties) = selected
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        properties.retain(|name, _| {
            fields.contains(&name.as_str())
                || required.is_some_and(|required| {
                    required
                        .iter()
                        .any(|value| value.as_str() == Some(name.as_str()))
                })
                || matches!(
                    name.as_str(),
                    "environment_id"
                        | "sandbox_permissions"
                        | "additional_permissions"
                        | "justification"
                        | "prefix_rule"
                )
        });
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ToolName;
    use serde_json::json;

    fn tool(name: &str, input_schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            tool_name: ToolName::plain(name),
            description: "Capability\n\nexec tool declaration: HUGE_SCHEMA".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(input_schema),
            input_schema_max_bytes: None,
            output_schema: None,
        }
    }

    #[test]
    fn curated_fields_follow_schema_and_keep_required_environment_and_approval_args() {
        let schema = json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string"},
                "required_extension": {"type": "string"},
                "environment_id": {"type": "string"},
                "sandbox_permissions": {"enum": ["use_default", "require_escalated"]},
                "additional_permissions": {"type": "object"},
                "justification": {"type": "string"},
                "prefix_rule": {"type": "array", "items": {"type": "string"}},
                "rare_optional": {"type": "string"}
            },
            "required": ["cmd", "required_extension"],
            "additionalProperties": false
        });
        let usage = tool_usage(&tool("exec_command", schema));
        assert!(usage.contains("cmd: string"));
        assert!(usage.contains("required_extension: string"));
        for name in [
            "environment_id",
            "sandbox_permissions",
            "additional_permissions",
            "justification",
            "prefix_rule",
        ] {
            assert!(usage.contains(&format!("{name}?:")), "{usage}");
        }
        assert!(!usage.contains("rare_optional"));
        assert!(!usage.contains("tty?:"));
        assert!(!usage.contains("login?:"));
    }

    #[test]
    fn native_aliases_and_image_detail_variants_do_not_invent_contracts() {
        let mut image = tool(
            "view_image",
            json!({
                "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]
            }),
        );
        assert!(!tool_usage(&image).contains("detail?:"));
        image.input_schema.as_mut().unwrap()["properties"]["detail"] =
            json!({"enum": ["high", "original"]});
        assert!(tool_usage(&image).contains("detail?: \"high\" | \"original\""));

        image.tool_name = ToolName::namespaced("mcp__other", "view_image");
        let usage = tool_usage(&image);
        assert!(!usage.contains("image(result)"));
        assert!(!usage.contains("HUGE_SCHEMA"));
    }

    #[test]
    fn one_shot_commands_do_not_advertise_resumable_sessions() {
        let command = tool(
            "exec_command",
            json!({
                "type": "object", "properties": {
                    "cmd": {"type": "string"}, "timeout_ms": {"type": "number"}
                }, "required": ["cmd"]
            }),
        );
        let usage = tool_usage(&command);
        assert!(usage.contains("timeout_ms?: number"));
        assert!(usage.contains("Timeout or cancellation terminates it"));
        assert!(!usage.contains("tty?:"));
        assert!(!usage.contains("session_id"));
        assert!(!usage.contains("write_stdin"));
    }

    #[test]
    fn catalog_guidance_respects_empty_overrides_and_mcp_results() {
        let mut deferred = tool("mcp_tool", json!({}));
        deferred.output_schema = Some(json!({"properties": {
            "content": {"type": "array", "items": {"type": "object"}},
            "isError": {"type": "boolean"}, "_meta": {"type": "object"}
        }}));
        let messages = CodeModeToolMessages {
            deferred_nested_tools_guidance: Some("CATALOG_GUIDANCE".to_string()),
            mcp_typescript_preamble: Some("type Custom = string;".to_string()),
            ..Default::default()
        };
        let prompt = build_notebook_tools_prompt(
            &[],
            &[deferred.clone()],
            &BTreeMap::new(),
            false,
            Some(&messages),
        );
        assert!(prompt.contains("CATALOG_GUIDANCE"));
        assert!(prompt.contains("type Custom = string;"));
        let empty = CodeModeToolMessages {
            deferred_nested_tools_guidance: Some(String::new()),
            mcp_typescript_preamble: Some(String::new()),
            ..Default::default()
        };
        let prompt =
            build_notebook_tools_prompt(&[], &[deferred], &BTreeMap::new(), false, Some(&empty));
        assert!(!prompt.contains("Additional tools in ALL_TOOLS"));
        assert!(!prompt.contains("Shared MCP Types"));
        let prompt =
            build_notebook_tools_prompt(&[], &[], &BTreeMap::new(), false, Some(&messages));
        assert!(!prompt.contains("CATALOG_GUIDANCE"));
        assert!(!prompt.contains("type Custom = string;"));
    }
}
