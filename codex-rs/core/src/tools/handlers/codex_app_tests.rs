use super::*;
use crate::tools::handlers::dynamic::DynamicToolHandler;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolExposure;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::models::ImageReference;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_tools::dynamic_tool_to_responses_api_tool;
use pretty_assertions::assert_eq;
use serde_json::json;

fn capture() -> DynamicToolNamespaceSpec {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../assets/tools/codex_app.json")).unwrap();
    serde_json::from_value(fixture["namespace"].clone()).unwrap()
}

#[test]
fn audited_descriptions_change_without_changing_schema_identity_or_deferral() {
    let namespace = capture();
    let mut shortened = 0;
    for tool in &namespace.tools {
        let DynamicToolNamespaceTool::Function(tool) = tool;
        let handler = DynamicToolHandler::new_in_namespace(&namespace, tool).unwrap();
        let mut expected = dynamic_tool_to_responses_api_tool(tool).unwrap();
        expected.defer_loading = None;
        if let Some(description) = compact_description(tool) {
            assert!(description.len() < tool.description.len());
            expected.description = description.to_string();
            shortened += 1;
        }
        assert_eq!(
            serde_json::to_value(handler.spec()).unwrap(),
            serde_json::to_value(ToolSpec::Namespace(ResponsesApiNamespace {
                name: namespace.name.clone(),
                description: namespace.description.clone(),
                tools: vec![ResponsesApiNamespaceTool::Function(expected)],
            }))
            .unwrap()
        );
        assert_eq!(
            handler.tool_name(),
            ToolName::namespaced("codex_app", tool.name.clone())
        );
        assert_eq!(
            handler.exposure(),
            if tool.defer_loading {
                ToolExposure::Deferred
            } else {
                ToolExposure::Direct
            }
        );
    }
    assert_eq!(shortened, 13);
}

#[test]
fn changed_unknown_or_other_clients_descriptions_stay_native() {
    let namespace = capture();
    let tool = namespace
        .tools
        .iter()
        .find_map(|tool| {
            let DynamicToolNamespaceTool::Function(tool) = tool;
            (tool.name == "list_threads").then_some(tool)
        })
        .unwrap();
    assert!(compact_description(tool).is_some());
    let mut changed = tool.clone();
    changed.description.push_str(" New native contract.");
    assert_eq!(compact_description(&changed), None);
    changed = tool.clone();
    changed.name = "future_tool".to_string();
    assert_eq!(compact_description(&changed), None);
    let mut other_namespace = namespace.clone();
    other_namespace.name = "other_client".to_string();
    for handler in [
        DynamicToolHandler::new(tool).unwrap(),
        DynamicToolHandler::new_in_namespace(&other_namespace, tool).unwrap(),
        DynamicToolHandler::new_in_namespace(&namespace, &changed).unwrap(),
    ] {
        let description = match handler.spec() {
            ToolSpec::Function(tool) => tool.description,
            ToolSpec::Namespace(namespace) => {
                let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0] else {
                    panic!("expected function");
                };
                tool.description.clone()
            }
            _ => panic!("expected dynamic function"),
        };
        assert_eq!(description, tool.description);
    }
}

fn payload() -> ToolPayload {
    ToolPayload::Function {
        arguments: "{}".to_string(),
    }
}

#[test]
fn successful_json_is_structured_for_notebook_but_native_history_stays_exact() {
    for text in [
        r#"{
  "status": "queued", "clientThreadId": "pending", "afterCursor": "cursor",
  "threads": [{"title": "Untrusted title", "needsInput": true}],
  "errors": [{"hostId": "remote", "error": "unreachable"}]
}"#,
        r#"[{"id":"artifact","path":"/workspace/file"}]"#,
    ] {
        let native = FunctionToolOutput::from_text(text.to_string(), Some(true));
        let expected_response = native.to_response_item("call", &payload());
        let expected_log = native.log_output();
        let compact = CodexAppToolOutput::new(native);
        assert_eq!(
            compact.code_mode_result(&payload()),
            serde_json::from_str::<Value>(text).unwrap()
        );
        assert_eq!(
            compact.to_response_item("call", &payload()),
            expected_response
        );
        assert_eq!(compact.log_output(), expected_log);
        assert!(compact.success_for_logging());
    }
}

#[test]
fn failure_plain_text_scalar_and_mixed_media_keep_native_notebook_results() {
    let image = FunctionCallOutputContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "data:image/png;base64,image".to_string(),
        },
        detail: None,
    };
    let audio = FunctionCallOutputContentItem::InputAudio {
        audio_url: "data:audio/wav;base64,audio".to_string(),
    };
    let json_text = FunctionCallOutputContentItem::InputText {
        text: json!({"status": "failed", "error": "approval required"}).to_string(),
    };
    for (success, body) in [
        (Some(false), vec![json_text.clone()]),
        (None, vec![json_text.clone()]),
        (Some(true), vec![json_text.clone(), image.clone()]),
        (Some(true), vec![image, audio]),
        (Some(true), vec![json_text.clone(), json_text]),
        (Some(true), Vec::new()),
        (
            Some(true),
            vec![FunctionCallOutputContentItem::InputText {
                text: "diagnostic\ntext".to_string(),
            }],
        ),
        (
            Some(true),
            vec![FunctionCallOutputContentItem::InputText {
                text: "42".to_string(),
            }],
        ),
        (
            Some(true),
            vec![FunctionCallOutputContentItem::InputText {
                text: "{incomplete JSON".to_string(),
            }],
        ),
    ] {
        let native = FunctionToolOutput::from_content(body, success);
        let expected = native.code_mode_result(&payload());
        let expected_response = native.to_response_item("call", &payload());
        let expected_success = native.success_for_logging();
        let compact = CodexAppToolOutput::new(native);
        assert_eq!(compact.code_mode_result(&payload()), expected);
        assert_eq!(
            compact.to_response_item("call", &payload()),
            expected_response
        );
        assert_eq!(compact.success_for_logging(), expected_success);
    }
}
