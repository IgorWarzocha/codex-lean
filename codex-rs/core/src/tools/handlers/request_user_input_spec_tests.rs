use super::*;
use codex_features::Feature;
use codex_features::Features;
use codex_tools::request_user_input_available_modes;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

fn plan_only_available_modes() -> Vec<ModeKind> {
    let mut features = Features::with_defaults();
    features.disable(Feature::DefaultModeRequestUserInput);
    request_user_input_available_modes(&features)
}

fn default_available_modes() -> Vec<ModeKind> {
    request_user_input_available_modes(&Features::with_defaults())
}

#[test_case(false, json!(["wait"]); "wait_only")]
#[test_case(true, json!(["wait", "async"]); "async_opt_in")]
fn question_schema_uses_one_shape_and_gates_delivery(
    async_enabled: bool,
    deliveries: serde_json::Value,
) {
    let ToolSpec::Function(spec) = create_request_user_input_tool("Ask".to_string(), async_enabled)
    else {
        panic!("expected function");
    };
    let schema = serde_json::to_value(spec.parameters).unwrap();
    assert_eq!(schema["properties"]["delivery"]["enum"], deliveries);
    assert_eq!(schema["required"], json!(["questions"]));
    assert_eq!(
        schema["properties"]["questions"]["items"]["required"],
        json!(["id", "header", "question"])
    );
    assert_eq!(
        schema["properties"]["questions"]["items"]["properties"]["options"]["items"]["required"],
        json!(["label", "description"])
    );
}

#[test]
fn question_arguments_default_to_wait_and_allow_free_text() {
    let args: RequestUserInputToolArgs = serde_json::from_value(json!({
        "questions": [{"id": "confirm", "header": "Confirm", "question": "Proceed?"}]
    }))
    .unwrap();
    assert_eq!(args.delivery, UserInputDelivery::Wait);
    let args = normalize_request_user_input_tool_args(args).unwrap();
    assert!(args.questions[0].is_other);
    assert!(args.questions[0].options.is_none());
}

#[test_case(json!({"questions": []}), "questions must not be empty"; "empty")]
#[test_case(json!({"questions": [{"id": "", "header": "", "question": "Proceed?"}]}), "question ids and text must not be empty"; "empty_id")]
#[test_case(json!({"questions": [{"id": "confirm", "header": "", "question": " "}]}), "question ids and text must not be empty"; "empty_text")]
#[test_case(json!({"questions": [{"id": "confirm", "header": "", "question": "Proceed?", "options": []}]}), "options must contain at least one non-empty answer"; "empty_options")]
#[test_case(json!({"questions": [{"id": "confirm", "header": "", "question": "Proceed?", "options": [{"label": " ", "description": ""}]}]}), "options must contain at least one non-empty answer"; "blank_option")]
fn invalid_questions_are_rejected(arguments: serde_json::Value, message: &str) {
    let args = serde_json::from_value(arguments).unwrap();
    assert_eq!(
        normalize_request_user_input_tool_args(args).unwrap_err(),
        message
    );
}

#[test]
fn invalid_delivery_and_unknown_arguments_are_rejected() {
    for arguments in [
        json!({"questions": [], "delivery": "surprise"}),
        json!({"questions": [], "mode": "async"}),
    ] {
        assert!(serde_json::from_value::<RequestUserInputToolArgs>(arguments).is_err());
    }
}

#[test]
fn request_user_input_unavailable_messages_respect_default_mode_feature_flag() {
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Plan, &default_available_modes()),
        None
    );
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Default, &plan_only_available_modes()),
        Some("request_user_input is unavailable in Default mode".to_string())
    );
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Default, &default_available_modes()),
        None
    );
}

#[test]
fn request_user_input_tool_description_mentions_available_modes() {
    assert_eq!(
        request_user_input_tool_description(&plan_only_available_modes(), false),
        "Ask the user; wait for answers; Plan mode only".to_string()
    );
    assert_eq!(
        request_user_input_tool_description(&default_available_modes(), false),
        "Ask the user; wait for answers; Default or Plan mode only".to_string()
    );
    assert_eq!(
        request_user_input_tool_description(&[ModeKind::Default], false),
        "Ask the user; wait for answers; Default mode only".to_string()
    );
}
