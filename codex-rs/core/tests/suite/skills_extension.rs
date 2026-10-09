use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core::config::Constrained;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecParams;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::ProcessId;
use codex_exec_server::RemoveOptions;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::SkillInvocationContributor;
use codex_extension_api::SkillInvocationInput;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::ExecutorSkillProvider;
use codex_skills_extension::HostSkillProvider;
use codex_skills_extension::SkillProvider;
use codex_skills_extension::SkillProviderSource;
use codex_skills_extension::SkillProviders;
use codex_skills_extension::SkillsExtensionConfig;
use codex_skills_extension::catalog::SkillAuthority;
use codex_skills_extension::catalog::SkillCatalog;
use codex_skills_extension::catalog::SkillCatalogEntry;
use codex_skills_extension::catalog::SkillPackageId;
use codex_skills_extension::catalog::SkillProviderError;
use codex_skills_extension::catalog::SkillReadResult;
use codex_skills_extension::catalog::SkillResourceId;
use codex_skills_extension::catalog::SkillSearchResult;
use codex_skills_extension::catalog::SkillSourceKind;
use codex_skills_extension::install;
use codex_skills_extension::install_with_providers;
use codex_skills_extension::provider::SkillListQuery;
use codex_skills_extension::provider::SkillProviderFuture;
use codex_skills_extension::provider::SkillReadRequest;
use codex_skills_extension::provider::SkillSearchRequest;
use codex_utils_path_uri::PathUri;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::apps_enabled_builder;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use core_test_support::skip_if_target_windows;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::test_env;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use sha1::Digest;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::Duration;
use tokio::time::Instant;
use toml::toml;
use wiremock::MockServer;

#[path = "skills_extension/cloud_skill_tests.rs"]
mod cloud_skill_tests;

#[path = "skills_extension/cloud_lifecycle_tests.rs"]
mod cloud_lifecycle_tests;

struct StaticSkillProvider {
    catalog: SkillCatalog,
    main_prompt_contents: Option<String>,
}

pub(super) struct CatalogSkillProvider {
    pub(super) catalog: SkillCatalog,
}

#[derive(Debug)]
enum CapturedExtensionEvent {
    Event,
    Warning(ExtensionWarning),
}

struct ChannelEventSink(std::sync::mpsc::Sender<CapturedExtensionEvent>);

impl ExtensionEventSink for ChannelEventSink {
    fn emit(&self, _event: Event) {
        let _ = self.0.send(CapturedExtensionEvent::Event);
    }

    fn emit_warning(&self, warning: ExtensionWarning) {
        let _ = self.0.send(CapturedExtensionEvent::Warning(warning));
    }
}

impl SkillProvider for StaticSkillProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        // Keep thread context empty so the catalog is exercised through the
        // production turn-input path, where the host snapshot is available.
        let catalog = if query.host_snapshot.is_some() {
            self.catalog.clone()
        } else {
            SkillCatalog::default()
        };
        Box::pin(async move { Ok(catalog) })
    }

    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        let result = self
            .main_prompt_contents
            .clone()
            .map(|contents| SkillReadResult {
                resource: request.resource,
                contents,
            })
            .ok_or_else(|| {
                SkillProviderError::new("production-flow catalog test does not read skills")
            });
        Box::pin(async move { result })
    }

    fn search(&self, _request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult> {
        Box::pin(async { Ok(SkillSearchResult::default()) })
    }
}

impl SkillProvider for CatalogSkillProvider {
    fn list(&self, _query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Ok(self.catalog.clone()) })
    }

    fn read<'a>(
        &'a self,
        _request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async {
            Err(SkillProviderError::new(
                "production-flow catalog test does not read skills",
            ))
        })
    }

    fn search(&self, _request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult> {
        Box::pin(async { Ok(SkillSearchResult::default()) })
    }
}

fn write_host_skills(codex_home: &std::path::Path, skills: &[(&str, &str)]) -> Result<()> {
    for (name, description) in skills {
        let skill_dir = codex_home.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# body\n"),
        )?;
    }
    Ok(())
}

fn catalog_extensions(
    executor_catalog: SkillCatalog,
    include_host_provider: bool,
) -> (
    Arc<codex_extension_api::ExtensionRegistry<Config>>,
    std::sync::mpsc::Receiver<CapturedExtensionEvent>,
) {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut extensions =
        ExtensionRegistryBuilder::<Config>::with_event_sink(Arc::new(ChannelEventSink(event_tx)));
    let mut providers =
        SkillProviders::new().with_executor_provider(Arc::new(CatalogSkillProvider {
            catalog: executor_catalog,
        }));
    if include_host_provider {
        providers = providers.with_host_provider(Arc::new(HostSkillProvider::new()));
    }
    install_with_providers(&mut extensions, providers, |config: &Config| {
        SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        }
    });
    (Arc::new(extensions.build()), event_rx)
}

async fn wait_for_analytics_events(
    server: &MockServer,
    event_type: &str,
    expected_count: usize,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| request.url.path() == "/codex/analytics-events/events")
            .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
            .flat_map(|payload| payload["events"].as_array().cloned().unwrap_or_default())
            .filter(|event| event["event_type"] == event_type)
            .collect::<Vec<_>>();
        if events.len() >= expected_count {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {event_type} analytics"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn configure_catalog_test(config: &mut Config) {
    config.include_skill_instructions = true;
    config
        .features
        .enable(Feature::ExecutorCapabilityDiscovery)
        .expect("executor capability discovery should be configurable in tests");
    // A user layer also discovers the real `$HOME/.agents/skills`. Use a temporary system layer so
    // exact catalog and omission assertions only see the skills written under this test's home.
    let system_config_path = config.codex_home.join("config.toml");
    config.config_layer_stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::System {
                file: system_config_path,
            },
            toml! { skills = { bundled = { enabled = false } } }.into(),
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("skills test config should be valid");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_host_repo_and_plugin_skill_instructions_remain_available() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "executor-backed repo skills require matching host and executor path conventions"
    );
    skip_if_no_network!(Ok(()));

    const HOST_SKILL_BODY: &str = "Use the host skill instructions.";
    const REPO_SKILL_BODY: &str = "Use the repository skill instructions.";
    const PLUGIN_SKILL_BODY: &str = "Use the legacy plugin skill instructions.";

    let server = responses::start_mock_server().await;
    let apps_server = AppsTestServer::mount_with_connector_name(&server, "Google Calendar").await?;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;

    let codex_home = Arc::new(TempDir::new()?);
    let host_skill_dir = codex_home.path().join("skills/host-search");
    std::fs::create_dir_all(&host_skill_dir)?;
    let host_skill_path = host_skill_dir.join("SKILL.md");
    std::fs::write(
        &host_skill_path,
        format!(
            "---\nname: host-search\ndescription: inspect host data\n---\n\n{HOST_SKILL_BODY}\n"
        ),
    )?;
    let host_skill_path = dunce::canonicalize(host_skill_path)?;
    let plugin_root = codex_home.path().join("plugins/cache/test/sample/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","description":"inspect sample data"}"#,
    )?;
    let plugin_skill_dir = plugin_root.join("skills/sample-search");
    std::fs::create_dir_all(&plugin_skill_dir)?;
    let plugin_skill_path = plugin_skill_dir.join("SKILL.md");
    std::fs::write(
        &plugin_skill_path,
        format!("---\ndescription: inspect sample data\n---\n\n{PLUGIN_SKILL_BODY}\n"),
    )?;
    let plugin_skill_path = dunce::canonicalize(plugin_skill_path)?;
    std::fs::write(
        plugin_root.join(".app.json"),
        r#"{"apps":{"sample":{"id":"calendar"}}}"#,
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nplugins = true\n\n[plugins.\"sample@test\"]\nenabled = true\n",
    )?;

    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install(&mut extensions, |config: &Config| SkillsExtensionConfig {
        include_instructions: config.include_skill_instructions,
        max_context_tokens: config.skill_max_context_tokens,
        bundled_skills_enabled: config.bundled_skills_enabled(),
        cloud_skill_enabled: config.cloud_skill_enabled,
        shadow_selection_enabled: config.features.enabled(Feature::SkillSearch),
    });
    let mut builder = test_codex()
        .with_home(Arc::clone(&codex_home))
        .with_extensions(Arc::new(extensions.build()))
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_workspace_setup(|cwd, fs| async move {
            let skill_dir = cwd.join(".agents/skills/repo-search");
            fs.create_directory(
                &PathUri::from_host_native_path(&skill_dir)?,
                CreateDirectoryOptions { recursive: true, follow_symlinks: true },
                /*sandbox*/ None,
            )
            .await?;
            fs.write_file(
                &PathUri::from_host_native_path(skill_dir.join("SKILL.md"))?,
                format!(
                    "---\nname: repo-search\ndescription: inspect repo data\n---\n\n{REPO_SKILL_BODY}\n"
                )
                .into_bytes(),
                Default::default(), /*sandbox*/ None,
            )
            .await?;
            Ok(())
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Apps)
                .expect("test config should allow feature update");
            config.chatgpt_base_url = apps_server.chatgpt_base_url;
        });
    let test = builder.build_with_auto_env(&server).await?;
    let repo_skill_path = test
        .fs()
        .canonicalize(
            &PathUri::from_abs_path(&test.config.cwd.join(".agents/skills/repo-search/SKILL.md")),
            /*sandbox*/ None,
        )
        .await?
        .to_abs_path()?
        .to_path_buf();

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: "use all skills".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Skill {
                name: "host-search".to_string(),
                path: host_skill_path.clone(),
            },
            UserInput::Skill {
                name: "repo-search".to_string(),
                path: repo_skill_path.clone(),
            },
            UserInput::Skill {
                name: "sample:sample-search".to_string(),
                path: plugin_skill_path.clone(),
            },
        ]))
        .await?;

    core_test_support::wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response.single_request();
    let developer_messages = request.message_input_texts("developer");
    let developer_text = developer_messages.join("\n\n");
    assert!(!developer_text.contains("### Available skills"));
    let user_text = request.message_input_texts("user").join("\n");
    for (name, path, body) in [
        ("host-search", &host_skill_path, HOST_SKILL_BODY),
        ("repo-search", &repo_skill_path, REPO_SKILL_BODY),
        (
            "sample:sample-search",
            &plugin_skill_path,
            PLUGIN_SKILL_BODY,
        ),
    ] {
        assert!(
            user_text.contains(&format!("<skill>\n<name>{name}</name>")),
            "expected injected skill `{name}` in user input: {user_text}"
        );
        assert!(
            user_text.contains(path.to_string_lossy().as_ref()),
            "expected path for `{name}` in user input: {user_text}"
        );
        assert!(
            user_text.contains(body),
            "expected body for `{name}` in user input: {user_text}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_plugin_skill_prompt_stays_bounded_without_skills_extension() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;

    let codex_home = Arc::new(TempDir::new()?);
    let plugin_root = codex_home
        .path()
        .join("plugins/cache/test/acme.tools/local");
    let skill_dir = plugin_root.join("skills/review");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        plugin_root.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"acme.tools","extensions":{"com.openai":{"interface":{"displayName":"Acme Developer Tools"}}}}"#,
    )?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: review\ndescription: Review code\n---\n\n{}\nAGENT_SKILL_TRUNCATED_TAIL\n",
            "x".repeat(9_000)
        ),
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nplugins = true\n\n[plugins.\"acme.tools@test\"]\nenabled = true\n",
    )?;
    let skill_path = dunce::canonicalize(skill_dir.join("SKILL.md"))?;
    let mut builder = test_codex().with_home(codex_home);
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Skill {
            name: "acme.tools:review".into(),
            path: skill_path,
        }]))
        .await?;
    let warning = core_test_support::wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::Warning(warning)
                if warning.message.contains("main prompt context limit")
        )
    })
    .await;
    core_test_support::wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let user_text = response
        .single_request()
        .message_input_texts("user")
        .join("\n");
    assert!(user_text.contains("acme.tools:review"));
    assert!(!user_text.contains("AGENT_SKILL_TRUNCATED_TAIL"));
    let EventMsg::Warning(warning) = warning else {
        unreachable!("wait_for_event matched an Agent skill truncation warning")
    };
    assert!(warning.message.contains("acme.tools:review"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_skill_prompt_precedes_plugin_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let codex_home = Arc::new(TempDir::new()?);
    let plugin_root = codex_home.path().join("plugins/cache/test/sample/local");
    let skill_dir = plugin_root.join("skills/sample-search");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","description":"inspect sample data"}"#,
    )?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: inspect sample data\n---\n\n# body\n",
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nplugins = true\n\n[plugins.\"sample@test\"]\nenabled = true\n",
    )?;
    let skill_path = dunce::canonicalize(skill_dir.join("SKILL.md"))?;
    let (extensions, _) =
        catalog_extensions(SkillCatalog::default(), /*include_host_provider*/ true);
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_extensions(extensions);
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Skill {
                name: "sample:sample-search".to_string(),
                path: skill_path,
            },
            UserInput::Mention {
                name: "sample".to_string(),
                path: "plugin://sample@test".to_string(),
            },
        ]))
        .await?;
    core_test_support::wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let input = response.single_request().input();
    let prompt_position = |expected: &str| {
        input
            .iter()
            .position(|item| {
                item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| {
                        part["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(expected))
                    })
                })
            })
            .unwrap_or_else(|| panic!("missing prompt containing `{expected}`: {input:?}"))
    };
    let skill_position = prompt_position("<skill>\n<name>sample:sample-search</name>");
    let plugin_position = prompt_position("Plugin `sample` capabilities:");
    assert!(
        skill_position < plugin_position,
        "host skill prompts should precede plugin instructions: {input:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn opted_in_executor_provider_skips_host_discovery_but_injects_discovered_skill() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "executor-backed repo skills require matching host and executor path conventions"
    );

    const AMBIENT_SKILL_NAME: &str = "ambient-repo";
    const AMBIENT_SKILL_BODY: &str = "AMBIENT_REPO_SKILL_SHOULD_NOT_BE_LOADED";
    const HOST_SKILL_NAME: &str = "ambient-home";
    const HOST_SKILL_DESCRIPTION: &str = "NON_REPO_HOST_SKILL_SHOULD_NOT_BE_LOADED";
    const EXECUTOR_SKILL_NAME: &str = "selected-executor";
    const EXECUTOR_SKILL_BODY: &str = "SELECTED_EXECUTOR_SKILL_REMAINS_AVAILABLE";
    const EXECUTOR_ROOT_ID: &str = "selected-capabilities";

    let server = responses::start_mock_server().await;
    let websocket_server = start_websocket_server(vec![vec![
        vec![ev_response_created("warm-1"), ev_completed("warm-1")],
        vec![ev_response_created("resp-1"), ev_completed("resp-1")],
        vec![ev_response_created("resp-2"), ev_completed("resp-2")],
    ]])
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    write_host_skills(
        codex_home.path(),
        &[(HOST_SKILL_NAME, HOST_SKILL_DESCRIPTION)],
    )?;
    let host_skill_path = codex_home
        .path()
        .join("skills")
        .join(HOST_SKILL_NAME)
        .join("SKILL.md");
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_executor_provider(Arc::new(
            ExecutorSkillProvider::new_with_restriction_product(
                Arc::new(EnvironmentManager::default_for_tests()),
                /*restriction_product*/ None,
            ),
        )),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_extensions(Arc::new(extensions.build()))
        .with_workspace_setup(|cwd, fs| async move {
            for (skill_dir, name, description, body) in [
                (
                    cwd.join(".agents/skills").join(AMBIENT_SKILL_NAME),
                    AMBIENT_SKILL_NAME,
                    "Ambient repository skill.",
                    AMBIENT_SKILL_BODY,
                ),
                (
                    cwd.join("selected-capabilities").join(EXECUTOR_SKILL_NAME),
                    EXECUTOR_SKILL_NAME,
                    "Selected executor skill.",
                    EXECUTOR_SKILL_BODY,
                ),
            ] {
                fs.create_directory(
                    &PathUri::from_host_native_path(&skill_dir)?,
                    CreateDirectoryOptions {
                        recursive: true,
                        follow_symlinks: true,
                    },
                    /*sandbox*/ None,
                )
                .await?;
                fs.write_file(
                    &PathUri::from_host_native_path(skill_dir.join("SKILL.md"))?,
                    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
                        .into_bytes(),
                    Default::default(),
                    /*sandbox*/ None,
                )
                .await?;
            }
            Ok(())
        })
        .with_config(|config| {
            configure_catalog_test(config);
            config
                .features
                .enable(Feature::SkipHostSkillDiscovery)
                .expect("host skill discovery opt-out should be configurable");
        });
    let test = builder.build_with_auto_env(&server).await?;
    let environment = test.executor_environment().selection().clone();
    let executor_skill_root = SelectedCapabilityRoot {
        id: EXECUTOR_ROOT_ID.to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: environment.environment_id.clone(),
            path: environment.cwd.join("selected-capabilities")?,
        },
    };
    let mut thread_extension_init = ExtensionDataInit::default();
    thread_extension_init.insert(vec![executor_skill_root]);
    let mut executor_config = test.config.clone();
    executor_config.model_provider.base_url = Some(format!("{}/v1", websocket_server.uri()));
    executor_config.model_provider.supports_websockets = true;
    let executor_thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            // Keep the trace fixture's legacy mode: paginated SQLite workers can close
            // spans through a different subscriber than this test's scoped collector.
            history_mode: Some(codex_protocol::protocol::ThreadHistoryMode::Legacy),
            environments: Some(vec![environment.clone().into_request()]),
            thread_extension_init,
            ..StartThreadOptions::new(executor_config)
        })
        .await?;
    let executor_skill_path = environment
        .cwd
        .join("selected-capabilities")?
        .join(EXECUTOR_SKILL_NAME)?
        .join("SKILL.md")?;
    let normalized_executor_skill_path = executor_skill_path
        .inferred_native_path_string()
        .replace('\\', "/");
    let normalized_executor_skill_path = normalized_executor_skill_path.trim_start_matches('/');
    let executor_skill_locator =
        format!("skill://{EXECUTOR_ROOT_ID}/{normalized_executor_skill_path}");

    let prewarm = tokio::time::timeout(
        Duration::from_secs(10),
        websocket_server.wait_for_request(/*connection_index*/ 0, /*request_index*/ 0),
    )
    .await?
    .body_json();
    assert_eq!(prewarm["generate"].as_bool(), Some(false));

    // Prewarm already materialized this file through DiscoverV1. Both turns must use that
    // snapshot for catalog metadata and instruction reads instead of rescanning the executor.
    test.fs()
        .remove(
            &executor_skill_path,
            RemoveOptions {
                recursive: false,
                force: false,
                follow_symlinks: true,
            },
            /*sandbox*/ None,
        )
        .await?;

    for turn in ["first", "next"] {
        executor_thread
            .thread
            .start_or_steer_turn(TurnInputRequest::user_input(vec![
                UserInput::Text {
                    text: format!(
                        "For the {turn} turn, use ${AMBIENT_SKILL_NAME}, ${HOST_SKILL_NAME}, and ${EXECUTOR_SKILL_NAME}."
                    ),
                    text_elements: Vec::new(),
                },
                UserInput::Skill {
                    name: HOST_SKILL_NAME.to_string(),
                    path: host_skill_path.clone(),
                },
                // Executor skills are authority-scoped resources, not host filesystem paths.
                UserInput::Mention {
                    name: EXECUTOR_SKILL_NAME.to_string(),
                    path: executor_skill_locator.clone(),
                },
            ]))
            .await?;
        core_test_support::wait_for_event(&executor_thread.thread, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }

    let requests = websocket_server.single_connection();
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().skip(1).enumerate() {
        let request = request.body_json();
        let message_text = |role| {
            request["input"]
                .as_array()
                .expect("response.create input array")
                .iter()
                .filter(|item| item["role"].as_str() == Some(role))
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let developer_text = message_text("developer");
        let user_text = message_text("user");
        assert!(!developer_text.contains("### Available skills"));
        assert!(
            user_text.contains(&format!("<skill>\n<name>{EXECUTOR_SKILL_NAME}</name>"))
                && user_text.contains(EXECUTOR_SKILL_BODY),
            "turn {index} should inject the discovered executor skill: {user_text}"
        );
        assert!(
            !developer_text.contains(AMBIENT_SKILL_NAME)
                && !developer_text.contains(HOST_SKILL_NAME)
                && !user_text.contains(AMBIENT_SKILL_BODY)
                && !user_text.contains(HOST_SKILL_DESCRIPTION)
                && !user_text.contains(&format!("<skill>\n<name>{AMBIENT_SKILL_NAME}</name>"))
                && !user_text.contains(&format!("<skill>\n<name>{HOST_SKILL_NAME}</name>")),
            "turn {index} must not load repository or non-repository host skills; developer: {developer_text}; user: {user_text}"
        );
    }

    executor_thread.thread.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    websocket_server.shutdown().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executor_only_provider_preserves_structured_repo_skill_without_discovery_opt_out()
-> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "structured host skill inputs require matching host and executor path conventions"
    );

    const AMBIENT_SKILL_NAME: &str = "ambient-repo";
    const AMBIENT_SKILL_BODY: &str = "AMBIENT_REPO_SKILL_REMAINS_AVAILABLE";

    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_executor_provider(Arc::new(
            ExecutorSkillProvider::new_with_restriction_product(
                Arc::new(EnvironmentManager::default_for_tests()),
                /*restriction_product*/ None,
            ),
        )),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_workspace_setup(|cwd, fs| async move {
            let skill_dir = cwd.join(".agents/skills").join(AMBIENT_SKILL_NAME);
            fs.create_directory(
                &PathUri::from_host_native_path(&skill_dir)?,
                CreateDirectoryOptions {
                    recursive: true,
                    follow_symlinks: true,
                },
                /*sandbox*/ None,
            )
            .await?;
            fs.write_file(
                &PathUri::from_host_native_path(skill_dir.join("SKILL.md"))?,
                format!(
                    "---\nname: {AMBIENT_SKILL_NAME}\ndescription: Ambient repository skill.\n---\n\n{AMBIENT_SKILL_BODY}\n"
                )
                .into_bytes(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        })
        .with_config(configure_catalog_test);
    let test = builder.build_with_auto_env(&server).await?;
    assert!(
        test.codex
            .inspect_selected_capability_roots()
            .ready_roots
            .is_empty(),
        "desktop sessions without selected capability roots must retain repository skills"
    );
    let ambient_skill_path = test
        .executor_environment()
        .cwd()
        .join(".agents/skills")
        .join(AMBIENT_SKILL_NAME)
        .join("SKILL.md")
        .to_path_buf();
    let ambient_skill_path = test
        .fs()
        .canonicalize(
            &PathUri::from_host_native_path(&ambient_skill_path)?,
            /*sandbox*/ None,
        )
        .await?
        .to_abs_path()?
        .to_path_buf();

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: format!("Use ${AMBIENT_SKILL_NAME}."),
                text_elements: Vec::new(),
            },
            UserInput::Skill {
                name: AMBIENT_SKILL_NAME.to_string(),
                path: ambient_skill_path,
            },
        ]))
        .await?;
    core_test_support::wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response.single_request();
    let user_text = request.message_input_texts("user").join("\n");
    assert!(
        user_text.contains(&format!("<skill>\n<name>{AMBIENT_SKILL_NAME}</name>"))
            && user_text.contains(AMBIENT_SKILL_BODY),
        "desktop's structured absolute-path skill selection must remain available: {user_text}"
    );

    Ok(())
}

#[derive(Clone, Copy)]
enum ExecutorReferenceRead {
    Allowed,
    Denied,
    Granted,
}

/// Live reference reads use the step's captured permissions after discovery.
#[test_case(ExecutorReferenceRead::Allowed; "full disk read")]
#[test_case(ExecutorReferenceRead::Denied; "denied reference")]
#[test_case(ExecutorReferenceRead::Granted; "approved escape is rejected")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executor_skill_tool_reads_references_under_current_permissions(
    read: ExecutorReferenceRead,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    if matches!(read, ExecutorReferenceRead::Denied) {
        skip_if_target_windows!(
            Ok(()),
            "restricted reads require the elevated Windows sandbox backend, unavailable in this fixture"
        );
    }

    const REFERENCE_CONTENTS: &str = "Live executor reference instructions.";
    let contents = REFERENCE_CONTENTS.to_string();
    let restricted_path_access = if matches!(read, ExecutorReferenceRead::Denied) {
        FileSystemAccessMode::Deny
    } else {
        FileSystemAccessMode::Read
    };
    let server = responses::start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_executor_provider(Arc::new(
            ExecutorSkillProvider::new_with_restriction_product(
                Arc::new(EnvironmentManager::default_for_tests()),
                /*restriction_product*/ None,
            ),
        )),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.truncation_policy = TruncationPolicyConfig::bytes(/*limit*/ 8192);
        })
        .with_config(configure_catalog_test);
    let test = builder.build_with_auto_env(&server).await?;
    let selection = test
        .codex
        .environment_selections()
        .await
        .into_iter()
        .next()
        .expect("thread should select an executor environment");
    let file_system = test.fs();
    let skill_dir = selection.cwd.join("skill")?;
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
    // Discovery retains the selected root's spelling, including macOS /var aliases.
    // Only permission entries should use the canonical path.
    let policy_skill_dir = file_system
        .canonicalize(&skill_dir, /*sandbox*/ None)
        .await?;
    for (name, contents) in [
        (
            "SKILL.md",
            "---\nname: skill\ndescription: Read executor references.\n---\n\nRead reference.md.\n",
        ),
        ("reference.md", contents.as_str()),
    ] {
        let directory = if name == "reference.md" && matches!(read, ExecutorReferenceRead::Granted)
        {
            &selection.cwd
        } else {
            &skill_dir
        };
        file_system
            .write_file(
                &directory.join(name)?,
                contents.as_bytes().to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
    }
    if matches!(read, ExecutorReferenceRead::Granted) {
        // The package directory must be listable for discovery. Put the reference outside it so
        // it still requires a separate grant; create the link on the executor, including remotely.
        let process = test
            .executor_environment()
            .environment()
            .get_exec_backend()
            .start(ExecParams {
                process_id: ProcessId::from("link-grant-reference"),
                metadata: None,
                argv: ["/bin/ln", "-s", "../reference.md", "skill/reference.md"]
                    .map(str::to_string)
                    .to_vec(),
                cwd: selection.cwd.clone(),
                env_policy: None,
                shell_snapshot: None,
                env: Default::default(),
                tty: false,
                pipe_stdin: false,
                arg0: None,
                sandbox: None,
                enforce_managed_network: false,
                managed_network: None,
                network_proxy: None,
            })
            .await?
            .process;
        let mut events = process.subscribe_events();
        loop {
            match events.recv().await? {
                ExecProcessEvent::Exited { exit_code, .. } => {
                    assert_eq!(exit_code, 0, "link the reference on the executor");
                    break;
                }
                ExecProcessEvent::Output(_) => {}
                event @ (ExecProcessEvent::Closed { .. } | ExecProcessEvent::Failed(_)) => {
                    panic!("reference symlink process did not exit: {event:?}")
                }
            }
        }
    }
    let package = format!(
        "skill://reference-root/{}",
        skill_dir
            .inferred_native_path_string()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let resource = format!("{package}/reference.md");
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(vec![SelectedCapabilityRoot {
        id: "reference-root".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: selection.environment_id.clone(),
            path: if matches!(read, ExecutorReferenceRead::Granted) {
                skill_dir.clone()
            } else {
                selection.cwd.clone()
            },
        },
    }]);
    let mut config = test.config.clone();
    if matches!(read, ExecutorReferenceRead::Granted) {
        config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
        config.features.enable(Feature::RequestPermissionsTool)?;
    }
    let (root_access, restricted_path) = if matches!(read, ExecutorReferenceRead::Granted) {
        (FileSystemAccessMode::Deny, policy_skill_dir.clone())
    } else {
        (
            FileSystemAccessMode::Read,
            policy_skill_dir.join("reference.md")?,
        )
    };
    config
        .permissions
        .set_permission_profile(PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::restricted(vec![
                FileSystemSandboxEntry::new(
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    root_access,
                ),
                FileSystemSandboxEntry::new(
                    FileSystemPath::Path {
                        path: restricted_path,
                    },
                    restricted_path_access,
                ),
            ]),
            NetworkSandboxPolicy::Restricted,
        ))?;
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![selection.into_request()]),
            thread_extension_init,
            ..StartThreadOptions::new(config)
        })
        .await?;
    let requested_permissions = RequestPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_path_uris(
            Some(vec![if matches!(read, ExecutorReferenceRead::Granted) {
                policy_skill_dir
                    .parent()
                    .expect("skill parent")
                    .join("reference.md")?
            } else {
                policy_skill_dir.join("reference.md")?
            }]),
            /*write*/ None,
        )),
        ..Default::default()
    };
    let mut response_sequence = Vec::new();
    if matches!(read, ExecutorReferenceRead::Granted) {
        // Separate responses ensure the denied read finishes before approval can be requested.
        response_sequence.push(sse(vec![
            ev_response_created("resp-before-grant"),
            responses::ev_custom_tool_call(
                "read-before-grant",
                "skills",
                &format!("read {package} {resource}"),
            ),
            ev_completed("resp-before-grant"),
        ]));
        response_sequence.push(sse(vec![
            ev_response_created("resp-grant"),
            responses::ev_function_call(
                "grant-reference",
                "request_permissions",
                &json!({"reason": "Read", "permissions": requested_permissions}).to_string(),
            ),
            ev_completed("resp-grant"),
        ]));
    }
    // The next model step captures any newly approved permissions.
    response_sequence.push(sse(vec![
        ev_response_created("resp-read"),
        responses::ev_custom_tool_call(
            "read-reference",
            "skills",
            &format!("read {package} {resource}"),
        ),
        ev_completed("resp-read"),
    ]));
    response_sequence.push(sse(vec![
        ev_response_created("resp-2"),
        ev_completed("resp-2"),
    ]));
    let response = responses::mount_sse_sequence(&server, response_sequence).await;
    thread
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Read the executor skill reference.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    if matches!(read, ExecutorReferenceRead::Granted) {
        let event = wait_for_event(&thread.thread, |event| {
            matches!(
                event,
                EventMsg::RequestPermissions(_) | EventMsg::TurnComplete(_)
            )
        })
        .await;
        let EventMsg::RequestPermissions(request) = event else {
            panic!("expected request_permissions before completion: {event:?}");
        };
        thread
            .thread
            .submit(Op::RequestPermissionsResponse {
                id: request.call_id,
                response: RequestPermissionsResponse {
                    permissions: requested_permissions,
                    scope: PermissionGrantScope::Turn,
                    strict_auto_review: false,
                },
            })
            .await?;
    }
    wait_for_event(&thread.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = response.requests();
    let output = skill_output(requests.last().expect("reference result"), "read-reference");
    if matches!(read, ExecutorReferenceRead::Granted) {
        assert!(skill_output(&requests[1], "read-before-grant").contains("Failed to read"));
    }
    if matches!(read, ExecutorReferenceRead::Granted) {
        assert!(output.contains("escapes its package"), "{output}");
    } else if matches!(read, ExecutorReferenceRead::Denied) {
        assert!(output.contains("Failed to read"));
    } else {
        assert!(output.starts_with(&contents), "{output}");
        assert!(output.contains("Sources:"));
    }

    Ok(())
}

/// Live executor prompts reject oversized resources without rewriting earlier injected content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_executor_skill_prompt_rejects_oversized_resource() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const FIRST_BODY: &str = "Instructions from the allowed live read.";
    let server = responses::start_mock_server().await;
    let environment = test_env().await?;
    let environment_id = environment.selection().environment_id.clone();
    let skill_path = environment.selection().cwd.join("SKILL.md")?;
    let file_system = environment.environment().get_filesystem();
    file_system
        .write_file(
            &skill_path,
            FIRST_BODY.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    let catalog = SkillCatalog {
        entries: vec![SkillCatalogEntry::new(
            SkillPackageId("skill://prompt-limit/live".to_string()),
            SkillAuthority::new(SkillSourceKind::Executor, "prompt-limit"),
            "live",
            "Read executor instructions.",
            SkillResourceId::environment(
                "skill://prompt-limit/live/SKILL.md",
                &environment_id,
                skill_path.clone(),
            ),
        )],
        warnings: Vec::new(),
    };
    let environment_manager = match environment.exec_server_url() {
        Some(url) => {
            EnvironmentManager::create_for_tests(
                Some(url.to_string()),
                /*local_runtime_paths*/ None,
            )
            .await
        }
        None => EnvironmentManager::default_for_tests(),
    };
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut extensions =
        ExtensionRegistryBuilder::<Config>::with_event_sink(Arc::new(ChannelEventSink(event_tx)));
    install_with_providers(
        &mut extensions,
        SkillProviders::new()
            .with_executor_provider(Arc::new(CatalogSkillProvider { catalog }))
            .with_executor_provider(Arc::new(
                ExecutorSkillProvider::new_with_restriction_product(
                    Arc::new(environment_manager),
                    /*restriction_product*/ None,
                ),
            )),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure_catalog_test);
    let test = builder.build_with_environment(&server, environment).await?;
    let first_response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    test.submit_turn("$live").await?;
    let expected_prompts = vec![format!(
        "<skill>\n<name>live</name>\n<path>skill://prompt-limit/live/SKILL.md</path>\n{FIRST_BODY}\n</skill>"
    )];
    let first_prompts = first_response
        .single_request()
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<skill>"))
        .collect::<Vec<_>>();
    assert_eq!(first_prompts, expected_prompts);

    file_system
        .write_file(
            &skill_path,
            vec![b'x'; 1024 * 1024 + 1],
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    let second_response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;
    test.submit_turn("$live").await?;
    let prompts = second_response
        .single_request()
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<skill>"))
        .collect::<Vec<_>>();
    assert_eq!(prompts, expected_prompts);
    let warnings = event_rx
        .try_iter()
        .filter_map(|event| match event {
            CapturedExtensionEvent::Warning(warning) => Some(warning.message),
            CapturedExtensionEvent::Event => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(warnings, vec![
        "Failed to load skill `live`: failed to read executor skill resource: skill resource exceeds content limit".to_string()
    ]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executor_skill_invocation_is_environment_scoped_and_deduplicated() -> Result<()> {
    skip_if_remote!(Ok(()), "executor fixture uses a host-local skill path");
    skip_if_no_network!(Ok(()));

    const SELECTED_RESOURCE: &str = "skill://selected-root/demo/SKILL.md";

    let server = responses::start_mock_server().await;
    let codex_home = Arc::new(TempDir::new()?);
    let skill_path = codex_home.path().join("executor-skill/SKILL.md");
    std::fs::create_dir_all(
        skill_path
            .parent()
            .expect("skill path should have a parent"),
    )?;
    std::fs::write(&skill_path, "executor skill contents\n")?;
    let skill_uri = PathUri::from_host_native_path(&skill_path)?;
    let catalog = SkillCatalog {
        entries: vec![
            SkillCatalogEntry::new(
                SkillPackageId("skill://other-root/demo".to_string()),
                SkillAuthority::new(SkillSourceKind::Executor, "other-root"),
                "other-environment-skill",
                "Skill from another environment.",
                SkillResourceId::environment(
                    "skill://other-root/demo/SKILL.md",
                    "other-environment",
                    skill_uri.clone(),
                ),
            ),
            SkillCatalogEntry::new(
                SkillPackageId("skill://selected-root/demo".to_string()),
                SkillAuthority::new(SkillSourceKind::Executor, "selected-root"),
                "selected-environment-skill",
                "Skill from the selected environment.",
                SkillResourceId::environment(SELECTED_RESOURCE, LOCAL_ENVIRONMENT_ID, skill_uri),
            ),
        ],
        warnings: Vec::new(),
    };
    let read_command = if cfg!(windows) {
        format!("Get-Content -LiteralPath \"{}\"", skill_path.display())
    } else {
        format!("cat {}", skill_path.display())
    };
    let command = json!({
        "cmd": read_command,
        "login": false,
    })
    .to_string();
    let response = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                responses::ev_function_call("executor-skill-first", "exec_command", &command),
                responses::ev_function_call("executor-skill-again", "exec_command", &command),
                ev_completed("resp-1"),
            ]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;

    let (extensions, _) = catalog_extensions(catalog, /*include_host_provider*/ false);
    let chatgpt_base_url = server.uri();
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_extensions(extensions)
        .with_config(move |config| {
            configure_catalog_test(config);
            config.chatgpt_base_url = chatgpt_base_url;
        });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn("Read the executor skill twice.").await?;

    for call_id in ["executor-skill-first", "executor-skill-again"] {
        let output = response
            .function_call_output_text(call_id)
            .expect("executor skill command should return output");
        assert!(
            output.contains("executor skill contents"),
            "command output: {output}"
        );
    }

    let events = wait_for_analytics_events(&server, "skill_invocation", /*expected_count*/ 1).await;
    assert_eq!(events.len(), 1, "executor skill should be counted once");
    assert_eq!(events[0]["skill_name"], "selected-environment-skill");
    assert_eq!(
        events[0]["skill_id"],
        format!("{:x}", sha1::Sha1::digest(SELECTED_RESOURCE.as_bytes()))
    );
    assert_eq!(events[0]["event_params"]["invoke_type"], "implicit");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_turn_uses_provider_host_catalog_and_core_snapshot_injection() -> Result<()> {
    let server = responses::start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    let skill_name = "snapshot-backed";
    let snapshot_description = "This description comes from Core's host skills snapshot.";
    write_host_skills(codex_home.path(), &[(skill_name, snapshot_description)])?;
    let skill_path = codex_home
        .path()
        .join("skills")
        .join(skill_name)
        .join("SKILL.md");
    let snapshot_contents = format!(
        "---\nname: {skill_name}\ndescription: {snapshot_description}\n---\n\nUse $calendar.\n"
    );
    std::fs::write(&skill_path, &snapshot_contents)?;
    let skill_resource = skill_path.to_string_lossy().into_owned();
    let provider_description = "This skill comes from the extension host provider.";
    let provider_contents = "# Provider instructions that Core must not inject.";
    let provider_catalog = SkillCatalog {
        entries: vec![
            SkillCatalogEntry::new(
                SkillPackageId(skill_resource.clone()),
                SkillAuthority::new(SkillSourceKind::Host, "host"),
                skill_name,
                provider_description,
                SkillResourceId::new(skill_resource.clone()),
            )
            .with_display_path(skill_resource.replace('\\', "/")),
        ],
        warnings: Vec::new(),
    };
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_host_provider(Arc::new(StaticSkillProvider {
            catalog: provider_catalog,
            main_prompt_contents: Some(provider_contents.to_string()),
        })),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let mut builder = apps_enabled_builder(apps_server.chatgpt_base_url)
        .with_home(Arc::clone(&codex_home))
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure_catalog_test);
    let test = builder.build_with_auto_env(&server).await?;
    wait_for_mcp_server(&test.codex, CODEX_APPS_MCP_SERVER_NAME).await?;

    test.submit_turn(&format!("Use ${skill_name}.")).await?;
    let request = response.single_request();
    let developer_texts = request.message_input_texts("developer");
    assert!(!developer_texts.iter().any(|text| text.contains(skill_name)));
    let user_text = request.message_input_texts("user").join("\n");
    assert!(user_text.contains(&snapshot_contents));
    assert!(!user_text.contains(provider_contents));
    let app_mentioned_events =
        wait_for_analytics_events(&server, "codex_app_mentioned", /*expected_count*/ 1).await;
    let app_mentioned_event = &app_mentioned_events[0];
    assert_eq!(
        app_mentioned_event["event_params"]["connector_id"],
        "calendar"
    );
    assert_eq!(
        app_mentioned_event["event_params"]["invoke_type"],
        "explicit"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_turn_suppresses_only_the_superseded_host_skill_prompt() -> Result<()> {
    #[derive(Default)]
    struct SkillInvocationRecorder(Mutex<Vec<String>>);

    impl SkillInvocationContributor for SkillInvocationRecorder {
        fn on_skill_invocation<'a>(
            &'a self,
            input: SkillInvocationInput<'a>,
        ) -> ExtensionFuture<'a, ()> {
            Box::pin(async move {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(input.skill_resource.to_owned());
            })
        }
    }

    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    write_host_skills(
        codex_home.path(),
        &[
            ("first-host", "First host skill."),
            ("second-host", "Second host skill."),
        ],
    )?;
    let first_skill_path = codex_home.path().join("skills/first-host/SKILL.md");
    let second_skill_path = codex_home.path().join("skills/second-host/SKILL.md");
    let first_host_contents =
        "---\nname: first-host\ndescription: First host skill.\n---\n\nFIRST_HOST_BODY\n";
    let second_host_contents =
        "---\nname: second-host\ndescription: Second host skill.\n---\n\nSECOND_HOST_BODY\n";
    std::fs::write(&first_skill_path, first_host_contents)?;
    std::fs::write(&second_skill_path, second_host_contents)?;
    let second_skill_path = dunce::canonicalize(second_skill_path)?;

    let source_kind = SkillSourceKind::Custom("test".to_string());
    let provider_resource = "skill://test/first-host/SKILL.md";
    let provider_contents = "FIRST_PROVIDER_BODY";
    let catalog = SkillCatalog {
        entries: vec![
            SkillCatalogEntry::new(
                SkillPackageId("test/first-host".to_string()),
                SkillAuthority::new(source_kind.clone(), "test"),
                "first-host",
                "Provider skill supersedes the matching host skill.",
                SkillResourceId::new(provider_resource),
            )
            .with_display_path(provider_resource),
        ],
        warnings: Vec::new(),
    };
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    let recorder = Arc::new(SkillInvocationRecorder::default());
    extensions.skill_invocation_contributor(recorder.clone());
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_provider(SkillProviderSource::new(
            source_kind,
            "test",
            Arc::new(StaticSkillProvider {
                catalog,
                main_prompt_contents: Some(provider_contents.to_string()),
            }),
        )),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure_catalog_test);
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("Use $first-host and $second-host.")
        .await?;

    let user_messages = response.single_request().message_input_texts("user");
    let skill_messages = user_messages
        .into_iter()
        .filter(|message| message.starts_with("<skill>"))
        .collect::<Vec<_>>();
    assert_eq!(
        skill_messages,
        vec![
            format!(
                "<skill>\n<name>second-host</name>\n<path>{}</path>\n{second_host_contents}\n</skill>",
                second_skill_path.display()
            ),
            format!(
                "<skill>\n<name>first-host</name>\n<path>{provider_resource}</path>\n{provider_contents}\n</skill>"
            ),
        ]
    );
    assert_eq!(
        *recorder
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![second_skill_path.display().to_string()]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_turn_warns_and_omits_unreadable_host_skill() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    write_host_skills(
        codex_home.path(),
        &[
            ("missing-host", "Missing host skill."),
            ("available-host", "Available host skill."),
        ],
    )?;
    let missing_skill_path =
        dunce::canonicalize(codex_home.path().join("skills/missing-host/SKILL.md"))?;
    let available_skill_path =
        dunce::canonicalize(codex_home.path().join("skills/available-host/SKILL.md"))?;
    let available_skill_contents = std::fs::read_to_string(&available_skill_path)?;
    let (extensions, _) =
        catalog_extensions(SkillCatalog::default(), /*include_host_provider*/ true);
    let mut builder = test_codex()
        .with_home(Arc::clone(&codex_home))
        .with_extensions(extensions)
        .with_config(configure_catalog_test);
    let test = builder.build_with_auto_env(&server).await?;

    std::fs::remove_file(&missing_skill_path)?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Skill {
                name: "missing-host".to_string(),
                path: missing_skill_path.clone(),
            },
            UserInput::Skill {
                name: "available-host".to_string(),
                path: available_skill_path.clone(),
            },
        ]))
        .await?;

    let mut warnings = Vec::new();
    loop {
        match core_test_support::wait_for_event(&test.codex, |_| true).await {
            EventMsg::Warning(warning) => warnings.push(warning.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let expected_warning_prefix = format!(
        "Failed to load skill missing-host at {}:",
        missing_skill_path.display()
    );
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&expected_warning_prefix),
        "expected unreadable skill warning, got {warnings:?}"
    );

    let skill_messages = response
        .single_request()
        .message_input_texts("user")
        .into_iter()
        .filter(|message| message.starts_with("<skill>"))
        .collect::<Vec<_>>();
    assert_eq!(
        skill_messages,
        vec![format!(
            "<skill>\n<name>available-host</name>\n<path>{}</path>\n{available_skill_contents}\n</skill>",
            available_skill_path.display()
        )]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_turn_keeps_full_snapshot_host_skill_prompt() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    let skill_dir = codex_home.path().join("skills").join("long-host");
    std::fs::create_dir_all(&skill_dir)?;
    let prompt_tail = "full host prompt tail";
    let skill_contents = format!(
        "---\nname: long-host\ndescription: Long host skill.\n---\n\n# Long host skill\n\n{}\n{prompt_tail}\n",
        "x".repeat(8_000)
    );
    std::fs::write(skill_dir.join("SKILL.md"), &skill_contents)?;
    let (extensions, _) =
        catalog_extensions(SkillCatalog::default(), /*include_host_provider*/ true);
    let mut builder = test_codex()
        .with_home(Arc::clone(&codex_home))
        .with_extensions(extensions)
        .with_config(|config| {
            configure_catalog_test(config);
            config
                .features
                .enable(Feature::SkipHostSkillDiscovery)
                .expect("host skill provider must override the discovery opt-out");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("Use $long-host.").await?;
    let user_text = response
        .single_request()
        .message_input_texts("user")
        .join("\n");

    assert!(user_text.contains(&skill_contents));
    assert_eq!(
        user_text.matches("<skill>\n<name>long-host</name>").count(),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_turn_keeps_core_host_injection_when_catalog_listings_are_disabled() -> Result<()>
{
    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    let skill_dir = codex_home.path().join("skills").join("long-host");
    std::fs::create_dir_all(&skill_dir)?;
    let prompt_tail = "full host prompt tail";
    let skill_contents = format!(
        "---\nname: long-host\ndescription: Long host skill.\n---\n\n# Long host skill\n\n{}\n{prompt_tail}\n",
        "x".repeat(8_000)
    );
    std::fs::write(skill_dir.join("SKILL.md"), &skill_contents)?;
    let (extensions, _) =
        catalog_extensions(SkillCatalog::default(), /*include_host_provider*/ true);
    let mut builder = test_codex()
        .with_home(Arc::clone(&codex_home))
        .with_extensions(extensions)
        .with_config(configure_catalog_test)
        .with_config(|config| {
            config.include_skill_instructions = false;
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("Use $long-host.").await?;
    let user_text = response
        .single_request()
        .message_input_texts("user")
        .join("\n");

    assert!(user_text.contains(&skill_contents));
    assert_eq!(
        user_text.matches("<skill>\n<name>long-host</name>").count(),
        1
    );

    Ok(())
}

fn skill_output(request: &responses::ResponsesRequest, call_id: &str) -> String {
    request
        .custom_tool_call_output_content_and_success(call_id)
        .expect("skills result")
        .0
        .expect("skills text")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_turn_discovers_skills_after_compaction_and_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));
    const COMPACT_PROMPT: &str = "Summarize discovered skills before resetting history.";
    const SUMMARY: &str = "Discovered cloud-search; rediscover skills when needed.";
    let server = responses::start_mock_server().await;
    let catalog = SkillCatalog {
        entries: vec![SkillCatalogEntry::new(
            SkillPackageId("cloud/cloud-search".to_string()),
            SkillAuthority::new(SkillSourceKind::Cloud, CODEX_APPS_MCP_SERVER_NAME),
            "cloud-search",
            "Search available company knowledge.",
            SkillResourceId::new("skill://codex_apps/cloud-search/SKILL.md"),
        )],
        warnings: Vec::new(),
    };
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_cloud_provider(Arc::new(CatalogSkillProvider { catalog })),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: true,
            shadow_selection_enabled: false,
        },
    );
    let extensions = Arc::new(extensions.build());
    let builder = || {
        test_codex()
            .with_direct_tools()
            .with_exec_server_url("none")
            .with_extensions(extensions.clone())
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                configure_catalog_test(config);
                config.cloud_skill_enabled = true;
                // Exercise local compaction without the backend's remote compact endpoint.
                config.model_provider.name = "Skills discovery compaction test".to_string();
                config.compact_prompt = Some(COMPACT_PROMPT.to_string());
            })
    };
    let list_response = |id: &str| {
        sse(vec![
            ev_response_created(id),
            responses::ev_custom_tool_call(id, "skills", "list"),
            ev_completed(id),
        ])
    };
    let responses = responses::mount_sse_sequence(
        &server,
        vec![
            list_response("before-compact"),
            responses::sse_completed("before-done"),
            sse(vec![
                ev_response_created("compact"),
                responses::ev_assistant_message("summary", SUMMARY),
                ev_completed("compact"),
            ]),
            list_response("after-compact"),
            responses::sse_completed("after-done"),
            list_response("after-resume"),
            responses::sse_completed("resumed-done"),
        ],
    )
    .await;
    let test = builder().build_with_auto_env(&server).await?;
    // No local attachment: cloud discovery must also work without an executor.
    test.submit_text_turn("Inspect the available skills.")
        .await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ContextCompacted(_))
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_text_turn("Inspect skills after compaction.")
        .await?;
    let resumed = builder().restart_with_auto_env(&server, &test).await?;
    assert_eq!(
        resumed.session_configured.thread_id,
        test.session_configured.thread_id
    );
    resumed
        .submit_text_turn("Inspect skills after resuming.")
        .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 7);
    assert!(requests[2].body_contains_text(COMPACT_PROMPT));
    assert!(requests[5].body_contains_text(SUMMARY));
    for (index, call_id) in [
        (1, "before-compact"),
        (4, "after-compact"),
        (6, "after-resume"),
    ] {
        assert_eq!(
            skill_output(&requests[index], call_id),
            "- cloud-search: Search available company knowledge.",
        );
    }
    // Discovery results belong to tool outputs, never an eagerly rendered standing catalog.
    for request in &requests {
        assert!(
            request
                .message_input_texts("developer")
                .iter()
                .all(|text| !text.contains("- cloud-search:"))
        );
    }
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
