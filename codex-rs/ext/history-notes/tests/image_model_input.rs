//! Verify history attachments survive native preparation into the next model request.

use std::sync::Arc;

use codex_config::types::ContextStrategy;
use codex_core::config::Config;
use codex_core::config::TokenBudgetConfig;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_history_notes_extension::install;
use codex_login::AuthHeaders;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use http::HeaderMap;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_images_reach_the_next_model_request() -> Result<(), Box<dyn std::error::Error>> {
    assert_history_images_reach_the_next_model_request(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_exec_history_results_reach_the_next_model_request()
-> Result<(), Box<dyn std::error::Error>> {
    assert_history_images_reach_the_next_model_request(true).await
}

#[expect(
    clippy::expect_used,
    reason = "assertions require successful fixture setup and parsed model-input fields"
)]
async fn assert_history_images_reach_the_next_model_request(
    nested: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/history/v2/read_item"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted_output": "opaque-history-text",
            "images": [
                {"data": PNG, "mime_type": "image/png", "detail": "original"},
                {"data": PNG, "mime_type": "image/png", "detail": "high"},
                {"data": PNG, "mime_type": "image/png", "detail": "auto"}
            ]
        })))
        .mount(&server)
        .await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                if nested {
                    responses::ev_custom_tool_call(
                        "read-history",
                        "exec",
                        "text(await Promise.all([1, 2].map(() => tools.history__read_item({window_id: 'window', item_id: 'item'}))));",
                    )
                } else { responses::ev_function_call_with_namespace(
                    "read-history",
                    "history",
                    "read_item",
                    &json!({"window_id": "window", "item_id": "item"}).to_string(),
                ) },
                responses::ev_completed("first-response"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("assistant", "done"),
                responses::ev_completed("second-response"),
            ]),
        ],
    )
    .await;
    let auth = CodexAuth::Headers(AuthHeaders::new(HeaderMap::new()));
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut extensions,
        AuthManager::from_auth_for_testing(auth.clone()),
    );
    let backend_url = format!("{}/backend-api/codex", server.uri());
    let test = test_codex()
        .with_context_strategy(ContextStrategy::Notes)
        .with_auth(auth)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.model_provider.name = "OpenAI".to_string();
            config.model_provider.base_url = Some(backend_url);
            if nested {
                config
                    .features
                    .enable(Feature::CodeModeOnly)
                    .expect("enable exec");
            }
            config.token_budget = Some(TokenBudgetConfig {
                use_history_notes_extension: true,
                ..TokenBudgetConfig::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Read the image from history.")
        .await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    let expected = json!([
            {"type": "encrypted_content", "encrypted_content": "opaque-history-text"},
            {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG}"), "detail": "original"},
            {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG}"), "detail": "high"},
            {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG}"), "detail": "auto"}
    ]);
    let body = requests[1].body_json();
    let outputs = body["input"]
        .as_array()
        .expect("input items")
        .iter()
        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "read-history")
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), if nested { 2 } else { 1 });
    let mut nested_ids = std::collections::BTreeSet::new();
    for item in outputs {
        let output = item["output"].as_array().expect("content items");
        if nested {
            assert!(
                output[0]["text"]
                    .as_str()
                    .expect("attribution")
                    .contains("history.read_item")
            );
            assert_eq!(&output[1..], expected.as_array().expect("expected items"));
            let attribution = output[0]["text"].as_str().expect("attribution");
            let (_, id) = attribution
                .split_once(", call_id ")
                .expect("nested call ID");
            nested_ids.insert(
                id.strip_suffix(": model-only output")
                    .expect("attribution suffix")
                    .to_string(),
            );
        } else {
            assert_eq!(&item["output"], &expected);
        }
    }
    if nested {
        let receipt = requests[1].custom_tool_call_output("read-history")["output"].to_string();
        assert!(receipt.contains("delivered_to_model"));
        assert!(!receipt.contains("opaque-history-text"));
        assert!(!receipt.contains(PNG));
        assert_eq!(nested_ids.len(), 2);
        for id in nested_ids {
            assert!(
                receipt.contains(&id),
                "parallel receipt must identify its model output"
            );
        }
    }
    Ok(())
}
