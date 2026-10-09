//! The model discovers one board entry and retrieves only the requested action contract.

use anyhow::Context;
use codex_agent_message_board_extension::AGENT_BOARD_TOOL_NAME;
use codex_features::CodeModeRuntime;
use codex_features::Feature;
use codex_protocol::openai_models::ToolMode;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[test_case::test_case(ToolMode::Direct; "direct")]
#[test_case::test_case(ToolMode::CodeMode; "mixed")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_help_keeps_action_schemas_out_of_initial_tools(
    mode: ToolMode,
) -> anyhow::Result<()> {
    exercise_help(mode).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local Deno executable"]
async fn notebook_board_help_and_actions_use_the_same_facade() -> anyhow::Result<()> {
    exercise_help(ToolMode::CodeModeOnly).await
}

async fn exercise_help(mode: ToolMode) -> anyhow::Result<()> {
    let nested = mode == ToolMode::CodeModeOnly;
    let server = responses::start_mock_server().await;
    let mut sequence = Vec::new();
    for (id, args) in [
        ("index", json!({"action":"help"})),
        ("post-help", json!({"action":"help","topic":"post"})),
        (
            "post",
            json!({"action":"post","new_channel_name":"decisions","text":"One facade, same board."}),
        ),
        (
            "read",
            json!({"action":"search_posts","channel_name":"decisions"}),
        ),
    ] {
        let call = if nested {
            responses::ev_custom_tool_call(
                id,
                "exec",
                &format!("text(await tools.collaboration__agent_board({args}));"),
            )
        } else {
            responses::ev_function_call_with_namespace(
                id,
                "collaboration",
                AGENT_BOARD_TOOL_NAME,
                &args.to_string(),
            )
        };
        sequence.push(responses::sse(vec![call, responses::ev_completed(id)]));
    }
    sequence.push(super::done());
    let mock = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(move |config| {
            super::configure(config);
            config.ephemeral = true;
            config.multi_agent_v2.message_board_in_memory = true;
            // Also protects the board-only messaging configuration after the tool rename.
            config.multi_agent_v2.disable_direct_message = true;
            config.code_mode.runtime = if nested {
                CodeModeRuntime::Notebook
            } else {
                CodeModeRuntime::V8
            };
            config.code_mode.deno_program = Some(
                std::env::var_os("DENO_PROGRAM")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| "deno".into()),
            );
            config
                .features
                .disable(Feature::CodeMode)
                .expect("configure test feature flags");
            config
                .features
                .disable(Feature::CodeModeOnly)
                .expect("configure test feature flags");
            config
                .features
                .disable(Feature::CodeModePrewarm)
                .expect("configure test feature flags");
        })
        .with_model_info_override("gpt-5.5", move |model| {
            model.tool_mode = Some(mode);
            model.multi_agent_version = Some(codex_protocol::protocol::MultiAgentVersion::V2);
        })
        .build(&server)
        .await?;
    test.submit_turn("Find Board help, post a decision, then read it.")
        .await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    let first = requests[0].body_json();
    let facade = responses::namespace_child_tool(&first, "collaboration", AGENT_BOARD_TOOL_NAME);
    if nested {
        assert!(facade.is_none());
        let prompt = requests[0].instructions_text();
        assert_eq!(
            prompt.matches("tools.collaboration__agent_board(").count(),
            1
        );
        assert!(!prompt.contains("max_chars_per_post"));
    } else {
        let facade = facade.context("compact board tool")?;
        let properties = facade["parameters"]["properties"]
            .as_object()
            .context("properties")?;
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["action"]
        );
        assert_eq!(facade["parameters"]["required"], json!(["action"]));
        assert_eq!(facade["parameters"]["additionalProperties"], true);
    }
    for old in [
        "create_channel",
        "get_channels",
        "list_threads",
        "search_posts",
        "read_thread",
        "read_post",
        "subscribe",
        "unsubscribe",
        "post",
    ] {
        assert!(
            responses::namespace_child_tool(&first, "collaboration", old).is_none(),
            "{old}"
        );
    }
    let output = |request: usize, id: &str| -> anyhow::Result<Value> {
        let text = if nested {
            requests[request]
                .custom_tool_call_output_content_and_success(id)
                .and_then(|(content, _)| content)
                .context("Notebook output")?
        } else {
            requests[request]
                .function_call_output_text(id)
                .context("board output")?
        };
        serde_json::from_str(&text).with_context(|| format!("{id}: {text}"))
    };
    let index = output(1, "index")?;
    assert!(!index.to_string().contains("parameters"));
    let help = output(2, "post-help")?;
    assert_eq!(help["action"], "post");
    assert!(help["parameters"]["properties"].get("text").is_some());
    assert!(help["parameters"]["properties"].get("cursor").is_none());
    let post = output(3, "post")?;
    assert_eq!(post["author"], "/root");
    let read = output(4, "read")?;
    assert_eq!(read["results"][0]["message_id"], post["message_id"]);
    assert_eq!(
        read["results"][0]["text_preview"],
        "One facade, same board."
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
