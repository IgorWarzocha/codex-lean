use super::*;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn windows_shell_guidance_description() -> String {
    format!("\n\n{}", windows_shell_guidance())
}

#[test]
fn exec_command_tool_matches_expected_spec() {
    let tool = create_exec_command_tool(CommandToolOptions {
        include_login_parameter: true,
        exec_permission_approvals_enabled: false,
    });

    let description = if cfg!(windows) {
        format!(
            "Run a shell command; output and session ID while running{}",
            windows_shell_guidance_description()
        )
    } else {
        "Run a shell command; output and session ID while running".to_string()
    };
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
            "shell".to_string(),
            JsonSchema::string(Some("Default user's shell".to_string())),
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
        (
            "login".to_string(),
            JsonSchema::boolean(Some("-l/-i shell semantics, default true".to_string())),
        ),
    ]);
    properties.extend(create_approval_parameters(
        /*exec_permission_approvals_enabled*/ false,
    ));

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "exec_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["cmd".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(unified_exec_output_schema().into()),
        })
    );
}

#[test]
fn write_stdin_tool_matches_expected_spec() {
    let tool = create_write_stdin_tool();

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

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "write_stdin".to_string(),
            description: "Write stdin or poll a running command for output".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["session_id".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(unified_exec_output_schema().into()),
        })
    );
}

#[test]
fn request_permissions_tool_includes_full_permission_schema() {
    let tool =
        create_request_permissions_tool("Request extra permissions for this turn.".to_string());

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

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "request_permissions".to_string(),
            description: "Request extra permissions for this turn.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["permissions".to_string()]),
                Some(false.into())
            ),
            output_schema: None,
        })
    );
}
