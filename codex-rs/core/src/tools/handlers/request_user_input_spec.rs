use codex_features::Features;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;

pub const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

pub(crate) fn request_user_input_async_available(
    source: &SessionSource,
    model: &ModelInfo,
    features: &Features,
    mode: ModeKind,
) -> bool {
    !source.is_non_root_agent()
        && request_user_input_mode_available(features, mode)
        && model.experimental_supported_tools.iter().any(|tool| {
            // Existing catalogs still use both names for the async question capability.
            matches!(
                tool.as_str(),
                "request_user_input_async" | "send_user_message_async"
            )
        })
}

pub(crate) fn request_user_input_mode_available(features: &Features, mode: ModeKind) -> bool {
    codex_tools::request_user_input_available_modes(features).contains(&mode)
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UserInputDelivery {
    #[default]
    Wait,
    Async,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestUserInputToolArgs {
    pub questions: Vec<RequestUserInputQuestion>,
    #[serde(default)]
    pub delivery: UserInputDelivery,
}

pub fn create_request_user_input_tool(description: String, async_enabled: bool) -> ToolSpec {
    let option_props = BTreeMap::from([
        (
            "label".to_string(),
            JsonSchema::string(Some("1-5 words".to_string())),
        ),
        (
            "description".to_string(),
            JsonSchema::string(Some("Choice impact, one sentence".to_string())),
        ),
    ]);

    let mut options_schema = JsonSchema::array(
        JsonSchema::object(
            option_props,
            Some(vec!["label".to_string(), "description".to_string()]),
            Some(false.into()),
        ),
        Some(
            "Suggested choices; no Other, automatic free text; omit for free-text-only".to_string(),
        ),
    );
    options_schema.min_items = Some(1);

    let question_props = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some("Wait answer key, snake_case".to_string())),
        ),
        (
            "header".to_string(),
            JsonSchema::string(Some("UI header, at most 12 characters".to_string())),
        ),
        (
            "question".to_string(),
            JsonSchema::string(Some("Self-contained question".to_string())),
        ),
        ("options".to_string(), options_schema),
    ]);

    let mut questions_schema = JsonSchema::array(
        JsonSchema::object(
            question_props,
            Some(vec![
                "id".to_string(),
                "header".to_string(),
                "question".to_string(),
            ]),
            Some(false.into()),
        ),
        Some("At most 3".to_string()),
    );
    questions_schema.min_items = Some(1);

    let mut properties = BTreeMap::from([("questions".to_string(), questions_schema)]);
    let deliveries = if async_enabled {
        vec!["wait".into(), "async".into()]
    } else {
        vec!["wait".into()]
    };
    properties.insert(
        "delivery".to_string(),
        JsonSchema::string_enum(deliveries, Some("Default wait; async only while continuing other work; replies arrive as user messages".to_string())),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["questions".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn request_user_input_unavailable_message(
    mode: ModeKind,
    available_modes: &[ModeKind],
) -> Option<String> {
    if available_modes.contains(&mode) {
        None
    } else {
        let mode_name = mode.display_name();
        Some(format!(
            "request_user_input is unavailable in {mode_name} mode"
        ))
    }
}

pub(crate) fn normalize_request_user_input_tool_args(
    mut args: RequestUserInputToolArgs,
) -> Result<RequestUserInputToolArgs, String> {
    if args.questions.is_empty() {
        return Err("questions must not be empty".to_string());
    }

    for question in &mut args.questions {
        if question.id.trim().is_empty() || question.question.trim().is_empty() {
            return Err("question ids and text must not be empty".to_string());
        }
        if let Some(options) = &question.options
            && (options.is_empty() || options.iter().any(|option| option.label.trim().is_empty()))
        {
            return Err("options must contain at least one non-empty answer".to_string());
        }
        question.is_other = true;
    }

    Ok(args)
}

pub fn request_user_input_tool_description(
    available_modes: &[ModeKind],
    async_enabled: bool,
) -> String {
    let allowed_modes = format_allowed_modes(available_modes);
    if async_enabled && available_modes.is_empty() {
        "Ask the user asynchronously; wait unavailable".to_string()
    } else if async_enabled {
        format!("Ask the user; wait or async; {allowed_modes} only")
    } else {
        format!("Ask the user; wait for answers; {allowed_modes} only")
    }
}

fn format_allowed_modes(available_modes: &[ModeKind]) -> String {
    let mode_names: Vec<&str> = available_modes
        .iter()
        .map(|mode| mode.display_name())
        .collect();

    match mode_names.as_slice() {
        [] => "no modes".to_string(),
        [mode] => format!("{mode} mode"),
        [first, second] => format!("{first} or {second} mode"),
        [..] => format!("modes: {}", mode_names.join(",")),
    }
}

#[cfg(test)]
#[path = "request_user_input_spec_tests.rs"]
mod tests;
