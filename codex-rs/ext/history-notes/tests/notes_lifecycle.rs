use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_config::types::ContextStrategy;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_history_notes_extension::install;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

#[expect(
    clippy::expect_used,
    reason = "static test authentication must be valid"
)]
fn notes_fixture(server: &MockServer) -> TestCodexBuilder {
    let auth = CodexAuth::from_external_chatgpt_tokens(
        "header.e30.signature",
        "account-123",
        Some("plus"),
    )
    .expect("test backend authentication");
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install(
        &mut extensions,
        AuthManager::from_auth_for_testing(auth.clone()),
    );
    let base_url = format!("{}/backend-api/codex", server.uri());
    test_codex()
        .with_context_strategy(ContextStrategy::Notes)
        .with_auth(auth)
        .with_extensions(Arc::new(extensions.build()))
        // The obsolete model experiment gate must not disable notes continuity.
        .with_model_info_override("gpt-5.5", |model| {
            model.supports_experimental_context = false
        })
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.supports_websockets = false;
            config.model_provider.stream_max_retries = Some(0);
            config.model_context_window = Some(128_000);
            config.model_auto_compact_token_limit = Some(100_000);
        })
}

async fn mock_notes(server: &MockServer, write_response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/write_file"))
        .respond_with(write_response)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": ""})))
        .mount(server)
        .await;
}

fn saved_notes() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"encrypted_output": "protected-note-receipt"}))
}

fn note_write(id: &str) -> String {
    responses::sse(vec![
        responses::ev_function_call_with_namespace(
            id,
            "notes",
            "write_file",
            r#"{"path":"checkpoint","text":"Active work and next steps"}"#,
        ),
        responses::ev_completed(id),
    ])
}

fn reply(id: &str) -> String {
    responses::sse(vec![
        responses::ev_assistant_message(id, id),
        responses::ev_completed(id),
    ])
}

#[expect(
    clippy::expect_used,
    reason = "test turns must emit events and settle within the timeout"
)]
async fn completed(test: &TestCodex) -> (TurnCompleteEvent, Vec<EventMsg>) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut events = Vec::new();
        loop {
            let event = test.codex.next_event().await.expect("turn event").msg;
            if let EventMsg::TurnComplete(completion) = event {
                return (completion, events);
            }
            events.push(event);
        }
    })
    .await
    .expect("turn must settle")
}

fn reset_count(events: &[EventMsg]) -> usize {
    events.iter().filter(|event| matches!(event,
        EventMsg::ItemCompleted(event) if matches!(event.item, TurnItem::ContextCompaction(_))
    )).count()
}

#[expect(
    clippy::expect_used,
    reason = "assert native requests carry valid context window metadata"
)]
fn window(request: &ResponsesRequest) -> String {
    // This UUID is the notes/history identity, not merely a transport request id.
    let metadata: Value = serde_json::from_str(
        &request
            .header("x-codex-turn-metadata")
            .expect("native turn metadata"),
    )
    .expect("turn metadata JSON");
    metadata["context_window_id"]
        .as_str()
        .expect("context window id")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_reuses_settled_notes_or_checkpoints_before_reset() -> TestResult {
    for already_fresh in [true, false] {
        let server = responses::start_mock_server().await;
        mock_notes(&server, saved_notes()).await;
        let mut bodies = if already_fresh {
            vec![note_write("save"), reply("settled")]
        } else {
            vec![reply("prior"), note_write("save"), reply("settled")]
        };
        bodies.push(reply("after"));
        let requests = responses::mount_sse_sequence(&server, bodies).await;
        let test = notes_fixture(&server)
            .with_pre_build_hook(|home| {
                let script = home.join("compact_hook.py");
                std::fs::write(&script, "import json\nimport sys\njson.load(sys.stdin)\n").unwrap();
                let hook = json!([{"matcher": "manual", "hooks": [{
                    "type": "command", "command": format!("python3 \"{}\"", script.display()),
                }]}]);
                std::fs::write(
                    home.join("hooks.json"),
                    json!({
                        "hooks": {"PreCompact": hook, "PostCompact": hook}
                    })
                    .to_string(),
                )
                .unwrap();
            })
            .with_config(trust_discovered_hooks)
            .build(&server)
            .await?;
        test.submit_text_turn("Original work").await?;
        let before_compact = requests.requests().len();
        test.codex.submit(Op::Compact).await?;
        let (completion, events) = completed(&test).await;
        assert!(completion.error.is_none(), "{completion:?}");
        assert_eq!(reset_count(&events), 1);
        assert_eq!(
            requests.requests().len() - before_compact,
            if already_fresh { 0 } else { 2 }
        );
        let hooks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                EventMsg::HookCompleted(event) => {
                    assert_eq!(event.run.status, HookRunStatus::Completed);
                    Some(event.run.event_name)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            hooks,
            vec![HookEventName::PreCompact, HookEventName::PostCompact]
        );
        if !already_fresh {
            assert!(
                requests.requests()[before_compact]
                    .body_contains_text("Checkpoint the active request")
            );
        }
        test.submit_text_turn("Continue original work").await?;
        let requests = requests.requests();
        let saved = &requests[if already_fresh { 1 } else { 2 }];
        assert_eq!(
            saved.function_call_output("save")["output"],
            json!([
                {"type": "encrypted_content", "encrypted_content": "protected-note-receipt"}
            ])
        );
        assert_ne!(window(&requests[0]), window(requests.last().unwrap()));
        assert!(!requests.last().unwrap().body_contains_text("Original work"));
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_accepts_a_clean_notes_retry_without_an_extra_checkpoint_turn() -> TestResult {
    for retry_during_compact in [false, true] {
        let server = responses::start_mock_server().await;
        let writes = Arc::new(AtomicUsize::new(0));
        let backend_writes = Arc::clone(&writes);
        mock_notes(&server, saved_notes()).await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/alpha/notes/v2/write_file"))
            .respond_with(move |_: &wiremock::Request| {
                if backend_writes.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(500)
                } else {
                    saved_notes()
                }
            })
            .with_priority(1)
            .mount(&server)
            .await;
        let mut bodies = Vec::new();
        if retry_during_compact {
            bodies.push(reply("prior-work"));
        }
        bodies.extend([
            note_write("failed-save"),
            note_write("retry-save"),
            reply("settled-retry"),
            reply("after"),
        ]);
        let requests = responses::mount_sse_sequence(&server, bodies).await;
        let test = notes_fixture(&server).build(&server).await?;
        test.submit_text_turn("Work that must survive the checkpoint")
            .await?;
        let before_compact = requests.requests().len();
        test.codex.submit(Op::Compact).await?;
        let (completion, events) = completed(&test).await;
        assert!(completion.error.is_none(), "{completion:?}");
        assert_eq!(reset_count(&events), 1);
        assert_eq!(writes.load(Ordering::SeqCst), 2);
        assert_eq!(
            requests.requests().len() - before_compact,
            if retry_during_compact { 3 } else { 0 },
            "reuse a settled retry, or only sample the requested checkpoint"
        );
        test.submit_text_turn("Continue after the retry").await?;
        let requests = requests.requests();
        assert_ne!(window(&requests[0]), window(requests.last().unwrap()));
        assert!(
            !requests
                .last()
                .unwrap()
                .body_contains_text("Work that must survive")
        );
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_refuses_a_partial_notes_batch_even_with_a_successful_sibling() -> TestResult {
    let server = responses::start_mock_server().await;
    mock_notes(&server, saved_notes()).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/write_file"))
        .and(wiremock::matchers::body_partial_json(
            json!({"path": "failed-note"}),
        ))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;
    let partial_batch = responses::sse(vec![
        responses::ev_function_call_with_namespace(
            "failed-save",
            "notes",
            "write_file",
            r#"{"path":"failed-note","text":"Part of checkpoint"}"#,
        ),
        responses::ev_function_call_with_namespace(
            "successful-sibling",
            "notes",
            "write_file",
            r#"{"path":"saved-note","text":"Other part of checkpoint"}"#,
        ),
        responses::ev_completed("partial-batch"),
    ]);
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            reply("prior"),
            partial_batch,
            reply("settled-partial"),
            reply("after"),
        ],
    )
    .await;
    let test = notes_fixture(&server).build(&server).await?;
    test.submit_text_turn("Retain this work").await?;
    test.codex.submit(Op::Compact).await?;
    let (completion, events) = completed(&test).await;
    assert!(completion.error.is_some());
    assert_eq!(reset_count(&events), 0);
    test.submit_text_turn("Continue without reset").await?;
    let requests = requests.requests();
    assert!(
        requests
            .iter()
            .all(|request| window(request) == window(&requests[0]))
    );
    assert!(
        requests
            .last()
            .unwrap()
            .body_contains_text("Retain this work")
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_cell_siblings_require_a_clean_retry_in_a_later_response() -> TestResult {
    for retry in [false, true] {
        let server = responses::start_mock_server().await;
        mock_notes(&server, saved_notes()).await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/alpha/notes/v2/write_file"))
            .and(wiremock::matchers::body_partial_json(
                json!({"path": "failed-note"}),
            ))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(1)
            .mount(&server)
            .await;
        let save_code =
            r#"await tools.notes__write_file({path: "saved-note", text: "Checkpoint"});"#;
        let mut bodies = vec![
            reply("prior"),
            responses::sse(vec![
                responses::ev_response_created("sibling-cells"),
                responses::ev_custom_tool_call(
                    "failed-cell",
                    "exec",
                    r#"try { await tools.notes__write_file({path: "failed-note", text: "Checkpoint"}); } catch {}"#,
                ),
                responses::ev_custom_tool_call("successful-sibling-cell", "exec", save_code),
                responses::ev_completed("sibling-cells"),
            ]),
        ];
        if retry {
            bodies.push(responses::sse(vec![
                responses::ev_custom_tool_call("retry-cell", "exec", save_code),
                responses::ev_completed("retry-cell"),
            ]));
        }
        bodies.extend([reply("settled"), reply("after")]);
        let requests = responses::mount_sse_sequence(&server, bodies).await;
        let test = notes_fixture(&server)
            .with_config(|config| {
                config
                    .features
                    .enable(codex_features::Feature::CodeMode)
                    .unwrap();
            })
            .build(&server)
            .await?;
        test.submit_text_turn("Retain this work").await?;
        test.codex.submit(Op::Compact).await?;
        let (completion, events) = completed(&test).await;
        assert_eq!(completion.error.is_none(), retry, "{completion:?}");
        assert_eq!(reset_count(&events), usize::from(retry));
        let write_count = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with("/write_file"))
            .count();
        assert_eq!(
            write_count,
            if retry { 3 } else { 2 },
            "both sibling cells must reach the notes executor"
        );
        test.submit_text_turn("Continue").await?;
        let requests = requests.requests();
        assert_eq!(
            window(&requests[0]) != window(requests.last().unwrap()),
            retry
        );
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_missing_or_untrusted_notes_refuse_reset() -> TestResult {
    for scenario in [
        "missing",
        "stale",
        "http-failure",
        "plaintext-success",
        "protected-error",
    ] {
        let server = responses::start_mock_server().await;
        let write_response = match scenario {
            "http-failure" => ResponseTemplate::new(500),
            "plaintext-success" => {
                ResponseTemplate::new(200).set_body_json(json!({"success": true}))
            }
            "protected-error" => ResponseTemplate::new(200).set_body_json(json!({
                "encrypted_output": "receipt", "error": "write failed",
            })),
            _ => saved_notes(),
        };
        mock_notes(&server, write_response).await;
        let attempts_write = !matches!(scenario, "missing" | "stale");
        let mut bodies = if scenario == "stale" {
            vec![
                note_write("old-save"),
                reply("old-settled"),
                reply("new-work"),
            ]
        } else {
            vec![reply("new-work")]
        };
        if attempts_write {
            bodies.push(note_write("failed-save"));
        }
        bodies.extend([reply("checkpoint-without-notes"), reply("after")]);
        let requests = responses::mount_sse_sequence(&server, bodies).await;
        let test = notes_fixture(&server).build(&server).await?;
        test.submit_text_turn("Work").await?;
        if scenario == "stale" {
            test.submit_text_turn("New work after those notes").await?;
        }
        let checkpoint_request = requests.requests().len();
        test.codex.submit(Op::Compact).await?;
        let (completion, events) = completed(&test).await;
        assert!(completion.error.is_some(), "{scenario}");
        assert_eq!(reset_count(&events), 0, "{scenario}");
        assert!(
            !events.iter().any(|event| matches!(event,
                EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::ContextCompaction(_))
            )),
            "{scenario}: failed notes must not start a reset"
        );
        assert!(
            events.iter().any(|event| matches!(event,
                EventMsg::Error(error) if error.message.contains("Context was not reset")
            )),
            "{scenario}: actionable checkpoint error"
        );
        assert!(
            requests.requests()[checkpoint_request]
                .body_contains_text("Checkpoint the active request")
        );
        test.submit_text_turn("Continue without reset").await?;
        let requests = requests.requests();
        assert!(
            requests
                .iter()
                .all(|request| window(request) == window(&requests[0])),
            "{scenario}"
        );
        assert!(requests.last().unwrap().body_contains_text("Work"));
        if scenario == "stale" {
            assert!(
                requests
                    .last()
                    .unwrap()
                    .body_contains_text("New work after those notes")
            );
        }
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_idle_requires_opt_in_and_selected_fresh_notes_and_keeps_input() -> TestResult {
    for (opt_in, fresh) in [(true, true), (false, true), (true, false)] {
        let server = responses::start_mock_server().await;
        mock_notes(&server, saved_notes()).await;
        let requests = responses::mount_sse_sequence(
            &server,
            vec![note_write("save"), reply("settled"), reply("after-idle")],
        )
        .await;
        let test = notes_fixture(&server).build(&server).await?;
        test.submit_text_turn("Prior window work").await?;
        let rollout_path = test
            .session_configured
            .rollout_path
            .clone()
            .expect("persisted rollout");
        test.codex.shutdown_and_wait().await?;
        // Age the host-authored settlement, not file mtime or session startup.
        let saved = std::fs::read_to_string(&rollout_path)?;
        let mut aged = Vec::new();
        let mut found = false;
        for line in saved.lines() {
            let mut line: Value = serde_json::from_str(line)?;
            if let Some(checkpoint) = line.pointer_mut("/payload/notes_checkpoint") {
                assert_eq!(
                    checkpoint["fresh"], true,
                    "real protected write must settle fresh"
                );
                checkpoint["settled_at_ms"] =
                    json!(checkpoint["settled_at_ms"].as_i64().unwrap() - 25 * 60_000);
                checkpoint["fresh"] = json!(fresh);
                found = true;
            }
            aged.push(line.to_string());
        }
        assert!(found, "trusted settlement persisted");
        std::fs::write(&rollout_path, format!("{}\n", aged.join("\n")))?;
        let mut builder = notes_fixture(&server).with_config(move |config| {
            config.context_idle_rollover_minutes = if opt_in { NonZeroU64::new(25) } else { None };
        });
        let resumed = builder
            .resume(&server, Arc::clone(&test.home), rollout_path)
            .await?;
        let text = "Preserve this exact incoming request\n  including whitespace.";
        let image_url = format!("data:image/png;base64,{PNG}");
        resumed
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![
                UserInput::Text {
                    text: text.into(),
                    text_elements: Vec::new(),
                },
                UserInput::Image {
                    image: ImageReference::Inline {
                        image_url: image_url.clone(),
                    },
                    detail: Some(ImageDetail::Original),
                },
            ]))
            .await?;
        let (completion, events) = completed(&resumed).await;
        assert!(completion.error.is_none(), "{completion:?}");
        assert_eq!(reset_count(&events), usize::from(opt_in && fresh));
        if opt_in && fresh {
            let turn_started = events
                .iter()
                .position(|event| matches!(event, EventMsg::TurnStarted(_)))
                .unwrap();
            let reset_started = events.iter().position(|event| matches!(event,
                EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::ContextCompaction(_))
            )).unwrap();
            assert!(
                turn_started < reset_started,
                "idle reset belongs to the admitted turn"
            );
        }
        let requests = requests.requests();
        assert_eq!(
            requests.len(),
            3,
            "idle must not add a model checkpoint request"
        );
        assert_eq!(
            window(&requests[0]) != window(&requests[2]),
            opt_in && fresh
        );
        assert!(requests[2].body_contains_text(&format!(
            "Current context window id: {}",
            window(&requests[2])
        )));
        assert!(
            requests[2]
                .message_input_texts("user")
                .iter()
                .any(|actual| actual == text)
        );
        assert_eq!(
            requests[2].message_input_image_urls("user"),
            vec![image_url]
        );
        let body = requests[2].body_json();
        let image = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .find(|span| span["type"] == "input_image")
            .unwrap();
        assert_eq!(image["detail"], "original");
        assert_eq!(
            requests[2].body_contains_text("Prior window work"),
            !(opt_in && fresh)
        );
        resumed.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_rollover_cancellation_retains_input_and_the_current_window() -> TestResult {
    for blocked_phase in ["PreCompact", "notes_hint"] {
        let server = responses::start_mock_server().await;
        mock_notes(&server, saved_notes()).await;
        let block_hint = Arc::new(AtomicBool::new(false));
        let hint_started = Arc::new(AtomicBool::new(false));
        let block = Arc::clone(&block_hint);
        let started = Arc::clone(&hint_started);
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
            .respond_with(move |_: &wiremock::Request| {
                let response = ResponseTemplate::new(200).set_body_json(json!({"text": ""}));
                if block.load(Ordering::SeqCst) {
                    started.store(true, Ordering::SeqCst);
                    response.set_delay(Duration::from_secs(60))
                } else {
                    response
                }
            })
            .with_priority(1)
            .mount(&server)
            .await;
        let requests = responses::mount_sse_sequence(
            &server,
            vec![note_write("save"), reply("settled"), reply("continued")],
        )
        .await;
        let test = notes_fixture(&server)
            .with_pre_build_hook(move |home| {
                let prompt_hook = home.join("prompt_hook.py");
                let count = home.join("prompt-count");
                std::fs::write(
                    &prompt_hook,
                    format!(
                        "import json, sys\njson.load(sys.stdin)\nwith open({:?}, 'a') as f: f.write('prompt\\n')\n",
                        count.to_str().unwrap()
                    ),
                )
                .unwrap();
                let mut hooks = json!({"UserPromptSubmit": [{"hooks": [{
                    "type": "command", "command": format!("python3 \"{}\"", prompt_hook.display())
                }]}]});
                if blocked_phase == "PreCompact" {
                    let compact_hook = home.join("compact_hook.py");
                    std::fs::write(
                        &compact_hook,
                        format!(
                            "import json, sys, time\njson.load(sys.stdin)\nopen({:?}, 'w').close()\ntime.sleep(60)\n",
                            home.join("compact-blocked").to_str().unwrap()
                        ),
                    )
                    .unwrap();
                    hooks["PreCompact"] = json!([{"matcher": "auto", "hooks": [{
                        "type": "command", "command": format!("python3 \"{}\"", compact_hook.display())
                    }]}]);
                }
                std::fs::write(home.join("hooks.json"), json!({"hooks": hooks}).to_string())
                    .unwrap();
            })
            .with_config(trust_discovered_hooks)
            .build(&server)
            .await?;
        test.submit_text_turn("Prior window evidence").await?;
        let rollout_path = test.session_configured.rollout_path.clone().unwrap();
        test.codex.shutdown_and_wait().await?;
        let saved = std::fs::read_to_string(&rollout_path)?;
        let mut aged = Vec::new();
        let mut found = false;
        for line in saved.lines() {
            let mut line: Value = serde_json::from_str(line)?;
            if let Some(checkpoint) = line.pointer_mut("/payload/notes_checkpoint") {
                assert_eq!(checkpoint["fresh"], true);
                checkpoint["settled_at_ms"] =
                    json!(checkpoint["settled_at_ms"].as_i64().unwrap() - 25 * 60_000);
                found = true;
            }
            aged.push(line.to_string());
        }
        assert!(found);
        std::fs::write(&rollout_path, format!("{}\n", aged.join("\n")))?;
        let mut builder = notes_fixture(&server).with_config(|config| {
            config.context_idle_rollover_minutes = NonZeroU64::new(25);
            trust_discovered_hooks(config);
        });
        let resumed = builder
            .resume(&server, Arc::clone(&test.home), rollout_path.clone())
            .await?;
        block_hint.store(blocked_phase == "notes_hint", Ordering::SeqCst);
        let text = "Accepted before rollover\n  preserve whitespace and attachments.";
        let image_url = format!("data:image/png;base64,{PNG}");
        resumed
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![
                UserInput::Text {
                    text: text.into(),
                    text_elements: Vec::new(),
                },
                UserInput::Image {
                    image: ImageReference::Inline {
                        image_url: image_url.clone(),
                    },
                    detail: Some(ImageDetail::Original),
                },
            ]))
            .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !(hint_started.load(Ordering::SeqCst)
                || resumed.home.path().join("compact-blocked").exists())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("rollover reached the blocked boundary");
        resumed.codex.submit(Op::Interrupt).await?;
        let events = tokio::time::timeout(Duration::from_secs(10), async {
            let mut events = Vec::new();
            loop {
                let event = resumed.codex.next_event().await.unwrap().msg;
                let aborted = matches!(event, EventMsg::TurnAborted(_));
                events.push(event);
                if aborted {
                    return events;
                }
            }
        })
        .await
        .expect("blocked rollover must interrupt promptly");
        assert_eq!(reset_count(&events), 0, "{blocked_phase}");
        assert_eq!(requests.requests().len(), 2, "no interrupted model request");
        let persisted = std::fs::read_to_string(&rollout_path)?;
        assert!(
            persisted.contains("Accepted before rollover"),
            "{blocked_phase}"
        );
        assert!(
            !persisted.lines().any(|line| {
                serde_json::from_str::<Value>(line).unwrap()["type"] == "compacted"
            }),
            "cancelled preparation must not publish a checkpoint"
        );
        assert_eq!(
            std::fs::read_to_string(resumed.home.path().join("prompt-count"))?
                .lines()
                .count(),
            2
        );
        block_hint.store(false, Ordering::SeqCst);
        resumed
            .submit_text_turn("Continue after interruption")
            .await?;
        let requests = requests.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            window(&requests[0]),
            window(&requests[2]),
            "{blocked_phase}"
        );
        assert!(requests[2].body_contains_text("Prior window evidence"));
        assert_eq!(
            requests[2]
                .message_input_texts("user")
                .iter()
                .filter(|actual| actual.as_str() == text)
                .count(),
            1,
            "accepted input must be recorded exactly once"
        );
        assert_eq!(
            requests[2].message_input_image_urls("user"),
            vec![image_url]
        );
        let body = requests[2].body_json();
        let image = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .find(|span| span["type"] == "input_image")
            .unwrap();
        assert_eq!(image["detail"], "original");
        assert_eq!(
            std::fs::read_to_string(resumed.home.path().join("prompt-count"))?
                .lines()
                .count(),
            3
        );
        resumed.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_rescue_preserves_evidence_and_retries_once_in_the_same_window() -> TestResult {
    for retry_succeeds in [true, false] {
        let server = responses::start_mock_server().await;
        mock_notes(&server, saved_notes()).await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/alpha/notes/v2/read_file"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"content": "Evidence: job 42 failed"})),
            )
            .mount(&server)
            .await;
        let overflow = || {
            responses::sse_failed(
                "overflow",
                "context_length_exceeded",
                "Input exceeds context",
            )
        };
        let read = responses::sse(vec![
            responses::ev_function_call_with_namespace(
                "evidence",
                "notes",
                "read_file",
                r#"{"path":"checkpoint"}"#,
            ),
            responses::ev_completed("read"),
        ]);
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                read,
                overflow(),
                reply("Readable summary: job 42 failed"),
                if retry_succeeds {
                    reply("recovered")
                } else {
                    overflow()
                },
            ],
        )
        .await;
        let test = notes_fixture(&server).build(&server).await?;
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Retain the current work".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        let (completion, events) = completed(&test).await;
        assert_eq!(completion.error.is_none(), retry_succeeds, "{completion:?}");
        assert_eq!(reset_count(&events), 1, "one readable emergency compaction");
        let requests = requests.requests();
        assert_eq!(
            requests.len(),
            4,
            "only one emergency summary and one retry"
        );
        assert!(
            requests
                .iter()
                .all(|request| window(request) == window(&requests[0]))
        );
        assert!(
            requests[2].body_contains_text("Evidence: job 42 failed"),
            "summarizer needs tool evidence"
        );
        assert!(requests[3].body_contains_text("Readable summary: job 42 failed"));
        assert!(requests[3].body_contains_text("Retain the current work"));
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_new_context_during_manual_checkpoint_does_not_reset_twice() -> TestResult {
    let server = responses::start_mock_server().await;
    mock_notes(&server, saved_notes()).await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            reply("prior"),
            note_write("save"),
            responses::sse(vec![
                responses::ev_function_call("rollover", "new_context", "{}"),
                responses::ev_completed("rollover"),
            ]),
            reply("after-native-reset"),
            reply("next-turn"),
        ],
    )
    .await;
    let test = notes_fixture(&server).build(&server).await?;
    test.submit_text_turn("Work").await?;
    test.codex.submit(Op::Compact).await?;
    let (completion, events) = completed(&test).await;
    assert!(completion.error.is_none(), "{completion:?}");
    assert!(
        !completion.notes_checkpoint.unwrap().fresh,
        "old-window notes must not bless the new window"
    );
    assert_eq!(reset_count(&events), 1);
    test.submit_text_turn("Continue").await?;
    let requests = requests.requests();
    assert_eq!(window(&requests[3]), window(&requests[4]));
    assert_ne!(window(&requests[2]), window(&requests[3]));
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
