use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::CapabilityRootLocation;
use codex_app_server_protocol::GrantedPermissionProfile;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::PermissionGrantScope;
use codex_app_server_protocol::PermissionsRequestApprovalResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SelectedCapabilityRoot;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_exec_server::CreateDirectoryOptions;
use codex_utils_path_uri::PathUri;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use core_test_support::skip_if_target_windows;
use futures::StreamExt;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

use super::analytics::mount_analytics_capture;
use super::analytics::wait_for_matching_analytics_event;

#[cfg(target_os = "macos")]
const READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(target_os = "macos"))]
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const SKILL_NAME: &str = "demo-plugin:deploy";
const SKILL_MARKER: &str = "EXECUTOR_SKILL_BODY_MARKER";
const LOCAL_SKILL_MARKER: &str = "LOCAL_SKILL_BODY_MARKER";
const REFERENCE_MARKER: &str = "EXECUTOR_SKILL_REFERENCE_MARKER";
const DENIED_SKILL_NAME: &str = "demo-plugin:denied";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutorSkillScenario {
    LargeCatalog,
    ExplicitOnly,
    RestrictedPermittedReference,
    RestrictedDeniedReference,
    RestrictedVisible,
}

#[tokio::test]
async fn selected_executor_root_reads_skills_on_demand_with_large_catalog() -> Result<()> {
    exercise_executor_skill(ExecutorSkillScenario::LargeCatalog).await
}

#[tokio::test]
async fn explicit_executor_skill_can_read_referenced_file() -> Result<()> {
    exercise_executor_skill(ExecutorSkillScenario::ExplicitOnly).await
}

#[tokio::test]
async fn restricted_executor_skill_can_read_permitted_reference() -> Result<()> {
    exercise_executor_skill(ExecutorSkillScenario::RestrictedPermittedReference).await
}

#[cfg(unix)]
#[tokio::test]
async fn restricted_executor_skill_rejects_package_escape_even_after_permission_approved()
-> Result<()> {
    exercise_executor_skill(ExecutorSkillScenario::RestrictedDeniedReference).await
}

#[tokio::test]
async fn restricted_executor_skill_is_listed_only_when_permitted() -> Result<()> {
    exercise_executor_skill(ExecutorSkillScenario::RestrictedVisible).await
}

async fn exercise_executor_skill(scenario: ExecutorSkillScenario) -> Result<()> {
    let restricted = matches!(
        scenario,
        ExecutorSkillScenario::RestrictedPermittedReference
            | ExecutorSkillScenario::RestrictedDeniedReference
            | ExecutorSkillScenario::RestrictedVisible
    );
    if restricted {
        skip_if_target_windows!(
            Ok(()),
            "the unelevated Windows sandbox cannot enforce restricted filesystem reads"
        );
    }
    if scenario == ExecutorSkillScenario::RestrictedDeniedReference {
        skip_if_remote!(Ok(()), "the external symlink fixture is host-local");
    }

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let (sandbox_config, permission_profile) = if restricted {
        (
            "default_permissions = \"workspace\"",
            "\n[permissions.workspace.filesystem.\":workspace_roots\"]\n\".\" = \"write\"\n\n[windows]\nsandbox = \"unelevated\"\n",
        )
    } else {
        ("sandbox_mode = \"read-only\"", "")
    };
    let (approval_policy, requested_permission_feature) =
        if scenario == ExecutorSkillScenario::RestrictedDeniedReference {
            (
                "on-request",
                "\n[features]\nrequest_permissions_tool = true\n",
            )
        } else {
            ("never", "")
        };
    let analytics_config = if scenario == ExecutorSkillScenario::ExplicitOnly {
        format!("chatgpt_base_url = \"{}\"", server.uri())
    } else {
        String::new()
    };
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
{analytics_config}
approval_policy = "{approval_policy}"
{sandbox_config}
model_provider = "mock_provider"

[skills]
include_instructions = true

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
{permission_profile}
{requested_permission_feature}
"#,
            server.uri()
        ),
    )?;
    if scenario == ExecutorSkillScenario::ExplicitOnly {
        mount_analytics_capture(&server, codex_home.path()).await?;
    }
    let local_skill_dir = codex_home.path().join("skills/local-deploy");
    std::fs::create_dir_all(&local_skill_dir)?;
    std::fs::write(
        local_skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {SKILL_NAME}\ndescription: Colliding local skill.\n---\n\n# Local deploy\n\n{LOCAL_SKILL_MARKER}\n"
        ),
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let auto_env = app_server.auto_env()?;
    let environment_id = auto_env.selection().environment_id.clone();
    let plugin_dir = auto_env.selection().cwd.join("plugin")?;
    let manifest_dir = plugin_dir.join(".codex-plugin")?;
    let skill_dir = plugin_dir.join("skills/deploy")?;
    let agents_dir = skill_dir.join("agents")?;
    let reference_dir = skill_dir.join("references")?;
    let file_system = auto_env.environment().get_filesystem();
    for directory in [&manifest_dir, &agents_dir, &reference_dir] {
        file_system
            .create_directory(
                directory,
                CreateDirectoryOptions {
                    recursive: true,
                    follow_symlinks: true,
                },
                /*sandbox*/ None,
            )
            .await?;
    }
    let manifest_path = manifest_dir.join("plugin.json")?;
    let skill_path = skill_dir.join("SKILL.md")?;
    let openai_yaml_path = agents_dir.join("openai.yaml")?;
    let reference_path = reference_dir.join("details.md")?;
    let reference_size = match scenario {
        ExecutorSkillScenario::LargeCatalog => 40 * 1024,
        ExecutorSkillScenario::RestrictedPermittedReference
        | ExecutorSkillScenario::RestrictedDeniedReference => 1024,
        ExecutorSkillScenario::ExplicitOnly | ExecutorSkillScenario::RestrictedVisible => 40 * 1024,
    };
    let allow_implicit_invocation = matches!(
        scenario,
        ExecutorSkillScenario::LargeCatalog | ExecutorSkillScenario::RestrictedVisible
    );
    let reference_contents = format!("{REFERENCE_MARKER}\n{}", "x".repeat(reference_size));
    tokio::try_join!(
        file_system.write_file(
            &manifest_path,
            br#"{"name":"demo-plugin"}"#.to_vec(),
            Default::default(), /*sandbox*/ None,
        ),
        file_system.write_file(
            &skill_path,
            format!(
                "---\nname: deploy\ndescription: Deploy through the executor.\n---\n\n# Deploy\n\n{SKILL_MARKER}\n\nRead references/details.md.\n"
            )
            .into_bytes(),
            Default::default(), /*sandbox*/ None,
        ),
        file_system.write_file(
            &openai_yaml_path,
            format!(
                "policy:\n  allow_implicit_invocation: {allow_implicit_invocation}\n"
            )
            .into_bytes(),
            Default::default(), /*sandbox*/ None,
        ),
        file_system.write_file(
            &reference_path,
            reference_contents.into_bytes(),
            Default::default(), /*sandbox*/ None,
        ),
    )?;
    #[cfg(unix)]
    if scenario == ExecutorSkillScenario::RestrictedDeniedReference {
        let external_reference_dir = codex_home.path().join("external-reference");
        std::fs::create_dir_all(&external_reference_dir)?;
        let external_reference = external_reference_dir.join("details.md");
        std::fs::write(
            &external_reference,
            format!(
                "DENIED_REFERENCE_MARKER\n{REFERENCE_MARKER}\n{}",
                "x".repeat(reference_size)
            ),
        )?;
        let reference_native_path = reference_path.to_abs_path()?;
        std::fs::remove_file(reference_native_path.as_path())?;
        std::os::unix::fs::symlink(external_reference, reference_native_path.as_path())?;
    }
    #[cfg(unix)]
    if scenario == ExecutorSkillScenario::RestrictedVisible && !auto_env.environment().is_remote() {
        let denied_skill_dir = codex_home.path().join("denied-skill");
        std::fs::create_dir_all(&denied_skill_dir)?;
        std::fs::write(
            denied_skill_dir.join("SKILL.md"),
            "---\nname: denied\ndescription: Skill outside the permitted workspace.\n---\n",
        )?;
        std::os::unix::fs::symlink(
            denied_skill_dir,
            plugin_dir.to_abs_path()?.join("skills/denied"),
        )?;
    }
    if scenario == ExecutorSkillScenario::LargeCatalog {
        futures::stream::iter(0..200)
            .map(|index| {
                let file_system = file_system.clone();
                let plugin_dir = plugin_dir.clone();
                async move {
                    let relative = format!("skills/skill-{index:03}");
                    let skill_dir = plugin_dir.join(&relative)?;
                    file_system
                        .create_directory(
                            &skill_dir,
                            CreateDirectoryOptions {
                                recursive: true,
                                follow_symlinks: true,
                            },
                            /*sandbox*/ None,
                        )
                        .await?;
                    file_system
                        .write_file(
                            &skill_dir.join("SKILL.md")?,
                            format!(
                                "---\nname: skill-{index:03}\ndescription: {}\n---\n",
                                "x".repeat(1_025)
                            )
                            .into_bytes(),
                            Default::default(),
                            /*sandbox*/ None,
                        )
                        .await?;
                    Ok::<(), anyhow::Error>(())
                }
            })
            .buffer_unordered(16)
            .try_collect::<Vec<_>>()
            .await?;
    }

    let authority_id = "demo-plugin@1";
    let locator = |path: &PathUri| {
        format!(
            "skill://{authority_id}/{}",
            path.inferred_native_path_string()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    };
    let package = locator(&skill_dir);
    let main_resource = locator(&skill_dir.join("SKILL.md")?);
    let reference_resource = locator(&reference_dir.join("details.md")?);
    let tool_response = |call_id: &str, tool: &str, arguments: serde_json::Value| {
        let command = if tool == "list" {
            "list".to_string()
        } else {
            format!(
                "read {}",
                arguments["resource"].as_str().expect("skill resource")
            )
        };
        responses::sse(vec![
            responses::ev_response_created(&format!("resp-{call_id}")),
            responses::ev_custom_tool_call(call_id, "skills", &command),
            responses::ev_completed(&format!("resp-{call_id}")),
        ])
    };
    let mut model_responses = vec![
        tool_response("list", "list", json!({})),
        tool_response(
            "main",
            "read",
            json!({
                "resource": main_resource.clone(),
            }),
        ),
        tool_response(
            "reference",
            "read",
            json!({
                "resource": reference_resource.clone(),
            }),
        ),
        responses::sse(vec![
            responses::ev_response_created("resp-done"),
            responses::ev_assistant_message("msg-done", "Done"),
            responses::ev_completed("resp-done"),
        ]),
    ];
    if scenario == ExecutorSkillScenario::RestrictedDeniedReference {
        let external_reference_dir = codex_home.path().join("external-reference");
        model_responses.insert(
            3,
            responses::sse(vec![
                responses::ev_response_created("resp-permissions"),
                responses::ev_function_call(
                    "permissions",
                    "request_permissions",
                    &json!({
                        "reason": "Read the approved skill reference",
                        "permissions": {
                            "file_system": {"read": [external_reference_dir]}
                        }
                    })
                    .to_string(),
                ),
                responses::ev_completed("resp-permissions"),
            ]),
        );
        model_responses.insert(
            4,
            tool_response(
                "approved-reference",
                "read",
                json!({
                    "resource": reference_resource.clone(),
                }),
            ),
        );
    }
    let response_mock = responses::mount_sse_sequence(&server, model_responses).await;

    timeout(READ_TIMEOUT, app_server.initialize()).await??;

    let request_id = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            config: match scenario {
                ExecutorSkillScenario::LargeCatalog => Some(HashMap::from([(
                    "skills.max_context_tokens".to_string(),
                    json!(1_000),
                )])),
                ExecutorSkillScenario::RestrictedPermittedReference
                | ExecutorSkillScenario::RestrictedDeniedReference => Some(HashMap::from([(
                    "tool_output_token_limit".to_string(),
                    json!(250),
                )])),
                ExecutorSkillScenario::ExplicitOnly | ExecutorSkillScenario::RestrictedVisible => {
                    None
                }
            },
            selected_capability_roots: Some(vec![SelectedCapabilityRoot {
                id: "demo-plugin@1".to_string(),
                location: CapabilityRootLocation::Environment {
                    environment_id,
                    path: plugin_dir,
                },
            }]),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(response)?;
    let thread_id = thread.id;

    let request_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: format!("Use ${SKILL_NAME}"),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    if scenario == ExecutorSkillScenario::RestrictedDeniedReference {
        let request =
            timeout(READ_TIMEOUT, app_server.read_stream_until_request_message()).await??;
        let ServerRequest::PermissionsRequestApproval { request_id, params } = request else {
            panic!("expected a skill reference permissions request, got {request:?}");
        };
        app_server
            .send_response(
                request_id,
                serde_json::to_value(PermissionsRequestApprovalResponse {
                    permissions: GrantedPermissionProfile {
                        network: None,
                        file_system: params.permissions.file_system,
                    },
                    scope: PermissionGrantScope::Turn,
                    strict_auto_review: None,
                })?,
            )
            .await?;
    }
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    if scenario == ExecutorSkillScenario::ExplicitOnly {
        for invocation_type in ["explicit", "implicit"] {
            let event = wait_for_matching_analytics_event(&server, READ_TIMEOUT, |event| {
                event["event_type"] == "skill_invocation"
                    && event["event_params"]["invoke_type"] == invocation_type
            })
            .await?;
            assert_eq!(event["event_params"]["plugin_id"], authority_id);
            assert_eq!(event["event_params"]["skill_scope"], "user");
        }
    }

    let requests = response_mock.requests();
    let request = &requests[0];
    assert!(
        request
            .message_input_texts("developer")
            .iter()
            .all(|text| !text.contains(SKILL_NAME))
    );
    let skill_fragments = request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<skill>"))
        .collect::<Vec<_>>();
    assert_eq!(1, skill_fragments.len());
    let skill_fragment = skill_fragments
        .first()
        .expect("executor skill instructions should be model-visible");
    assert!(skill_fragment.contains(&format!("<name>{SKILL_NAME}</name>")));
    assert!(skill_fragment.contains(SKILL_MARKER));
    assert!(!skill_fragment.contains(LOCAL_SKILL_MARKER));
    match scenario {
        ExecutorSkillScenario::LargeCatalog | ExecutorSkillScenario::RestrictedVisible => {
            assert!(!skill_fragment.contains("<resource_access>"));
        }
        ExecutorSkillScenario::ExplicitOnly
        | ExecutorSkillScenario::RestrictedPermittedReference
        | ExecutorSkillScenario::RestrictedDeniedReference => {
            let resource_access = skill_fragment
                .split_once("<resource_access>")
                .and_then(|(_, rest)| rest.split_once("</resource_access>"))
                .map(|(metadata, _)| serde_json::from_str::<serde_json::Value>(metadata))
                .transpose()?
                .expect("explicit executor skill should include resource access metadata");
            assert_eq!(
                resource_access,
                json!({
                    "authority": {"kind": "executor", "id": authority_id},
                    "package": package,
                    "main_resource": main_resource,
                })
            );
        }
    }
    let skill_output = |index: usize, call_id: &str| {
        requests[index]
            .custom_tool_call_output_content_and_success(call_id)
            .expect("skills command output")
            .0
            .expect("skills command text")
    };
    let list_output = skill_output(1, "list");
    match scenario {
        ExecutorSkillScenario::LargeCatalog => {
            assert!(
                list_output.contains("maximum is 49152 bytes"),
                "{list_output}"
            );
        }
        ExecutorSkillScenario::RestrictedVisible => {
            assert!(list_output.contains(SKILL_NAME));
            assert!(list_output.contains("Deploy through the executor."));
            assert!(!list_output.contains(DENIED_SKILL_NAME));
        }
        ExecutorSkillScenario::ExplicitOnly
        | ExecutorSkillScenario::RestrictedPermittedReference
        | ExecutorSkillScenario::RestrictedDeniedReference => {
            assert!(!list_output.contains("Deploy through the executor."));
        }
    }
    let main_output = skill_output(2, "main");
    assert!(main_output.contains(SKILL_MARKER), "{main_output}");
    assert!(!main_output.contains(LOCAL_SKILL_MARKER));
    assert!(main_output.contains("Skill paths"));

    let reference_output = skill_output(3, "reference");
    if scenario == ExecutorSkillScenario::RestrictedDeniedReference {
        assert!(
            reference_output.contains("Failed to read skill resource"),
            "{reference_output}"
        );
        assert!(!reference_output.contains("DENIED_REFERENCE_MARKER"));
        let approved_reference = skill_output(5, "approved-reference");
        // Filesystem permission does not authorize crossing a skill package boundary.
        assert!(
            approved_reference.contains("skill resource escapes its package"),
            "{approved_reference}"
        );
        assert!(!approved_reference.contains("DENIED_REFERENCE_MARKER"));
        assert!(!approved_reference.contains(REFERENCE_MARKER));
        return Ok(());
    }

    let expected_contents = format!("{REFERENCE_MARKER}\n{}", "x".repeat(reference_size));
    assert!(
        reference_output.contains(&expected_contents),
        "{reference_output}"
    );
    assert!(reference_output.contains("Sources:"));

    if scenario == ExecutorSkillScenario::RestrictedPermittedReference {
        // On-demand selected instructions are atomic even with a small general tool budget.
        let changed_contents = format!("CHANGED_REFERENCE\n{}", "y".repeat(reference_size));
        file_system
            .write_file(
                &reference_path,
                changed_contents.clone().into_bytes(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
        let refreshed = responses::mount_sse_sequence(
            &server,
            vec![
                tool_response(
                    "fresh-reference",
                    "read",
                    json!({"resource": reference_resource}),
                ),
                responses::sse(vec![responses::ev_completed("resp-refreshed-done")]),
            ],
        )
        .await;
        timeout(
            READ_TIMEOUT,
            app_server.start_turn_and_wait_for_completion(TurnStartParams {
                thread_id,
                input: vec![UserInput::Text {
                    text: "Read the changed reference.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            }),
        )
        .await??;
        let (fresh_output, _) = refreshed
            .requests()
            .last()
            .expect("fresh reference continuation")
            .custom_tool_call_output_content_and_success("fresh-reference")
            .expect("fresh reference output");
        let fresh_output = fresh_output.expect("fresh reference text");
        assert!(fresh_output.contains(&changed_contents));
        assert!(!fresh_output.contains(REFERENCE_MARKER));
    }

    Ok(())
}
