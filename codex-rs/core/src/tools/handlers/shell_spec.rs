use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandToolOptions {
    pub include_login_parameter: bool,
    pub exec_permission_approvals_enabled: bool,
}

#[cfg(test)]
pub fn create_exec_command_tool(options: CommandToolOptions) -> ToolSpec {
    create_exec_command_tool_with_environment_id(
        options,
        /*include_environment_id*/ false,
        /*include_shell_parameter*/ true,
        /*include_windows_shell_guidance*/ cfg!(windows),
    )
}

pub(crate) fn create_exec_command_tool_with_environment_id(
    options: CommandToolOptions,
    include_environment_id: bool,
    include_shell_parameter: bool,
    include_windows_shell_guidance: bool,
) -> ToolSpec {
    let yield_time_ms_description = if cfg!(windows) {
        "Wait ms, default 10000, Windows range 10000-30000; immediate return when finished"
    } else {
        "Wait ms, default 10000, range 250-30000"
    };
    let mut properties = BTreeMap::from([
        ("cmd".to_string(), JsonSchema::string(None)),
        (
            "workdir".to_string(),
            JsonSchema::string(Some("Default turn cwd".to_string())),
        ),
        (
            "tty".to_string(),
            JsonSchema::boolean(Some("PTY for stdin interaction; default pipes".to_string())),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(yield_time_ms_description.to_string())),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some("Default 10000, subject to policy caps".to_string())),
        ),
    ]);
    if include_shell_parameter {
        properties.insert(
            "shell".to_string(),
            JsonSchema::string(Some("Default user's shell".to_string())),
        );
    }
    if options.include_login_parameter {
        properties.insert(
            "login".to_string(),
            JsonSchema::boolean(Some("-l/-i shell semantics, default true".to_string())),
        );
    }
    if include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "ID from <environment_context>, defaults to primary environment".to_string(),
            )),
        );
    }
    properties.extend(create_approval_parameters(
        options.exec_permission_approvals_enabled,
    ));

    ToolSpec::Function(ResponsesApiTool {
        name: "exec_command".to_string(),
        description: if include_windows_shell_guidance {
            format!(
                "Run a shell command; output and session ID while running\n\n{}",
                windows_shell_guidance()
            )
        } else {
            "Run a shell command; output and session ID while running".to_string()
        },
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["cmd".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(unified_exec_output_schema().into()),
    })
}

pub fn create_write_stdin_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "session_id".to_string(),
            JsonSchema::number(Some("Session ID from exec_command".to_string())),
        ),
        (
            "chars".to_string(),
            JsonSchema::string(Some(
                "Stdin bytes for tty=true sessions; empty or omitted to poll".to_string(),
            )),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(
                "Wait ms; writes default 250, cap 30000; empty polls default 5000-300000"
                    .to_string(),
            )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some("Default 10000, subject to policy caps".to_string())),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "write_stdin".to_string(),
        description: "Write stdin or poll a running command for output".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["session_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(unified_exec_output_schema().into()),
    })
}

pub fn create_request_permissions_tool(description: String) -> ToolSpec {
    let properties = BTreeMap::from([
        ("reason".to_string(), JsonSchema::string(None)),
        (
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "ID from <environment_context>, defaults to primary environment".to_string(),
            )),
        ),
        ("permissions".to_string(), permission_profile_schema()),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "request_permissions".to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["permissions".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn request_permissions_tool_description() -> String {
    "Request filesystem or network permissions; wait for client-approved subset\nRelative paths from selected environment cwd; automatic grants for later shell-like commands this turn, or session if approved at session scope"
        .to_string()
}

fn unified_exec_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "chunk_id": {
                "type": "string",
                "description": "Output chunk ID"
            },
            "wall_time_seconds": {
                "type": "number",
                "description": "Seconds spent waiting for output"
            },
            "exit_code": {
                "type": "number",
                "description": "Exit code if finished"
            },
            "session_id": {
                "type": "number",
                "description": "Pass to write_stdin while running"
            },
            "original_token_count": {
                "type": "number",
                "description": "Approximate tokens before truncation"
            },
            "output": {
                "type": "string",
                "description": "Output, possibly truncated"
            }
        },
        "required": ["wall_time_seconds", "output"],
        "additionalProperties": false
    })
}

fn create_approval_parameters(
    exec_permission_approvals_enabled: bool,
) -> BTreeMap<String, JsonSchema> {
    let mut sandbox_permission_values = vec![json!("use_default")];
    if exec_permission_approvals_enabled {
        sandbox_permission_values.push(json!("with_additional_permissions"));
    }
    sandbox_permission_values.push(json!("require_escalated"));
    let sandbox_permissions_description = if exec_permission_approvals_enabled {
        "Default use_default; with_additional_permissions requires additional_permissions; require_escalated unsandboxed"
    } else {
        "Default use_default; require_escalated unsandboxed"
    };

    let mut properties = BTreeMap::from([
        (
            "sandbox_permissions".to_string(),
            JsonSchema::string_enum(
                sandbox_permission_values,
                Some(sandbox_permissions_description.to_string()),
            ),
        ),
        (
            "justification".to_string(),
            JsonSchema::string(Some(
                "Approval question for require_escalated only".to_string(),
            )),
        ),
        (
            "prefix_rule".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some(r#"Reusable cmd approval prefix, only with require_escalated"#.to_string()),
            ),
        ),
    ]);

    if exec_permission_approvals_enabled {
        let mut additional_permissions = permission_profile_schema();
        additional_permissions.description = Some(
            "Sandboxed access for this command, only with with_additional_permissions".to_string(),
        );
        properties.insert("additional_permissions".to_string(), additional_permissions);
    }

    properties
}

fn permission_profile_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            ("network".to_string(), network_permissions_schema()),
            ("file_system".to_string(), file_system_permissions_schema()),
        ]),
        /*required*/ None,
        Some(false.into()),
    )
}

fn network_permissions_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([(
            "enabled".to_string(),
            JsonSchema::boolean(Some("Default false".to_string())),
        )]),
        /*required*/ None,
        Some(false.into()),
    )
}

fn file_system_permissions_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "read".to_string(),
                JsonSchema::array(
                    JsonSchema::string(/*description*/ None),
                    Some("Absolute paths".to_string()),
                ),
            ),
            (
                "write".to_string(),
                JsonSchema::array(
                    JsonSchema::string(/*description*/ None),
                    Some("Absolute paths".to_string()),
                ),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    )
}

fn windows_shell_guidance() -> &'static str {
    r#"Windows safety:
- One shell end-to-end for delete or move; no PowerShell paths passed to cmd /c, batch builtins, or another shell; prefer Remove-Item or Move-Item with -LiteralPath, no string-built file-operation commands
- Before recursive delete or move: verify resolved absolute targets within intended workspace or explicitly named directory, including computed paths
- Start-Process background helpers or services: -WindowStyle Hidden unless explicitly requested visible; visible only for user-facing interactive tools"#
}

#[cfg(test)]
#[path = "shell_spec_tests.rs"]
mod tests;
