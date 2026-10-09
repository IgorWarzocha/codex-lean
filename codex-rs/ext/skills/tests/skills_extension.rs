use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirementsToml;
use codex_exec_server::FileSystemEnvironmentAccessor;
use codex_exec_server::LOCAL_FS;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::FunctionCallError;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::SkillInvocationInput;
use codex_extension_api::SkillInvocationKind;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolPayload;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnStartInput;
use codex_extension_api::WorldStateContributionInput;
use codex_models_manager::model_info::model_info_from_slug;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SkillScope;
use codex_protocol::protocol::TruncationPolicy;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::user_input::UserInput;
use codex_skills::SkillMetadata;
use codex_skills_extension::HostSkillsLoadInput;
use codex_skills_extension::HostSkillsService;
use codex_skills_extension::HostSkillsSnapshot;
use codex_skills_extension::InjectedHostSkillPrompts;
use codex_skills_extension::SkillLoadOutcome;
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
use codex_skills_extension::install_with_providers_and_metrics;
use codex_skills_extension::provider::SkillListQuery;
use codex_skills_extension::provider::SkillProvider;
use codex_skills_extension::provider::SkillProviderFuture;
use codex_skills_extension::provider::SkillReadRequest;
use codex_skills_extension::provider::SkillSearchRequest;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;
use pretty_assertions::assert_eq;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[path = "skills_extension/shadow_task_context_tests.rs"]
mod shadow_task_context_tests;

#[path = "skills_extension/progressive_tests.rs"]
mod progressive_tests;

static NEXT_CODEX_HOME_ID: AtomicUsize = AtomicUsize::new(0);
const DEMO_SKILL_CONTENTS: &str =
    "---\nname: demo\ndescription: Demo skill.\n---\n# Demo\n\nUse the demo skill.\n";

fn catalog_model_info() -> ModelInfo {
    ModelInfo {
        context_window: None,
        max_context_window: None,
        include_skills_usage_instructions: false,
        ..model_info_from_slug("test-model")
    }
}

async fn start_registered_turn(
    registry: &codex_extension_api::ExtensionRegistry<TestConfig>,
    session_store: &ExtensionData,
    thread_store: &ExtensionData,
    turn_id: &str,
) {
    let turn_store = ExtensionData::new(turn_id);
    let mode = codex_protocol::config_types::CollaborationMode {
        mode: codex_protocol::config_types::ModeKind::Default,
        settings: codex_protocol::config_types::Settings {
            model: "test".to_string(),
            reasoning_effort: None,
            developer_instructions: None,
        },
    };
    for contributor in registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(TurnStartInput {
                turn_id,
                collaboration_mode: &mode,
                token_usage_at_turn_start: None,
                session_store,
                thread_store,
                turn_store: &turn_store,
            })
            .await;
    }
}

#[tokio::test]
async fn installed_extension_uses_host_service_snapshot() -> TestResult {
    let codex_home = test_codex_home();
    let skill_path = codex_home.join("skills").join("demo").join("SKILL.md");
    std::fs::create_dir_all(
        skill_path
            .parent()
            .ok_or("skill path should have a parent")?,
    )?;
    std::fs::write(&skill_path, DEMO_SKILL_CONTENTS)?;
    let mut config = default_config();
    config.shadow_selection_enabled = true;

    let mut builder = ExtensionRegistryBuilder::new();
    install(&mut builder, skills_extension_config);
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let session_source = SessionSource::Cli;
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &session_source,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;

    let skill_path = AbsolutePathBuf::try_from(skill_path)?;
    let skill_path_string = skill_path.to_string_lossy().into_owned();
    let mut outcome = SkillLoadOutcome::default();
    outcome.skills.push(SkillMetadata {
        name: "demo".to_string(),
        description: "Demo skill.".to_string(),
        short_description: None,
        interface: None,
        dependencies: None,
        policy: None,
        path_to_skills_md: PathUri::from_abs_path(&skill_path),
        scope: SkillScope::User,
        plugin_id: None,
        remote_plugin_id: None,
    });
    let loaded_skills = Arc::new(outcome);
    let skill_prompt_path = skill_path_string.replace('\\', "/");
    let turn_store = ExtensionData::new("turn-1");
    turn_store.insert(HostSkillsSnapshot::new(Arc::clone(&loaded_skills)));

    let fragments = registry.turn_input_contributors()[0]
        .contribute(
            TurnInputContext {
                turn_id: "turn-1".to_string(),
                user_input: vec![UserInput::Text {
                    text: "$demo".to_string(),
                    text_elements: Vec::new(),
                }],
                environments: Vec::new(),
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &turn_store,
        )
        .await;

    let expected_skill = format!(
        "<skill>\n<name>demo</name>\n<path>{skill_prompt_path}</path>\n{DEMO_SKILL_CONTENTS}\n</skill>"
    );
    assert_eq!(
        vec![("user", expected_skill)],
        fragments
            .iter()
            .map(|fragment| (fragment.role(), fragment.render()))
            .collect::<Vec<_>>()
    );
    let injected_host_skill_prompts = turn_store
        .get::<InjectedHostSkillPrompts>()
        .ok_or("host skill prompt marker should be set")?;
    assert!(injected_host_skill_prompts.contains_path(&skill_path_string));

    std::fs::remove_dir_all(codex_home)?;
    Ok(())
}

#[tokio::test]
async fn shadow_selection_uses_host_catalog_when_instructions_are_disabled() -> TestResult {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(StaticSkillProvider {
        catalog: SkillCatalog {
            entries: vec![test_entry(
                SkillSourceKind::Host,
                "host",
                "host/lint-fix",
                "lint-fix/SKILL.md",
            )],
            warnings: Vec::new(),
        },
        read_requests: Arc::new(Mutex::new(Vec::new())),
        list_calls: Some(Arc::clone(&list_calls)),
        fail_first_list: false,
    });
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-skills-extension",
            env!("CARGO_PKG_VERSION"),
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )?;
    let mut builder = ExtensionRegistryBuilder::new();
    install_with_providers_and_metrics(
        &mut builder,
        SkillProviders::new().with_host_provider(provider),
        Some(metrics.clone()),
        skills_extension_config,
    );
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let mut config = default_config();
    config.include_instructions = false;
    config.shadow_selection_enabled = true;
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &SessionSource::Cli,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    let turn_store = ExtensionData::new("turn-1");
    turn_store.insert(HostSkillsSnapshot::new(Arc::new(
        SkillLoadOutcome::default(),
    )));

    let sections = registry.context_contributors()[0]
        .contribute_world_state(WorldStateContributionInput {
            previous_world_state: None,
            model_info: &catalog_model_info(),
            thread_id: codex_protocol::ThreadId::new(),
            turn_id: "turn-1",
            environments: &[],
            ready_selected_capability_roots: &[],
            executor_capability_discovery: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: &turn_store,
            step_store: &turn_store,
        })
        .await;
    let fragments = registry.turn_input_contributors()[0]
        .contribute(
            TurnInputContext {
                turn_id: "turn-1".to_string(),
                user_input: vec![UserInput::Text {
                    text: "Fix lint errors.".to_string(),
                    text_elements: Vec::new(),
                }],
                environments: Vec::new(),
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &turn_store,
        )
        .await;

    assert!(sections.is_empty());
    assert!(fragments.is_empty());
    let snapshots = shadow_task_context_tests::collect_shadow_observations(
        &metrics,
        "codex.skills.shadow_selection.catalog_entries",
        /*expected*/ 12,
    )
    .await?;
    let catalog_entry_counts = snapshots
        .iter()
        .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|metric| metric.name() == "codex.skills.shadow_selection.catalog_entries")
        .flat_map(|metric| match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                .data_points()
                .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum),
            data => panic!("unexpected shadow catalog metric data: {data:?}"),
        })
        .collect::<Vec<_>>();

    assert!(
        catalog_entry_counts.iter().all(|count| *count == 1.0),
        "every shadow selector should see the cached host skill: {catalog_entry_counts:?}"
    );
    assert_eq!(1, list_calls.load(Ordering::Relaxed));
    Ok(())
}

#[tokio::test]
async fn shadow_lru_selector_recovers_a_skill_invoked_on_an_earlier_turn() -> TestResult {
    let provider = Arc::new(StaticSkillProvider {
        catalog: SkillCatalog {
            entries: vec![test_entry(
                SkillSourceKind::Host,
                "host",
                "host/lint-fix",
                "lint-fix/SKILL.md",
            )],
            warnings: Vec::new(),
        },
        read_requests: Arc::new(Mutex::new(Vec::new())),
        list_calls: None,
        fail_first_list: false,
    });
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-skills-extension",
            env!("CARGO_PKG_VERSION"),
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )?;
    let mut builder = ExtensionRegistryBuilder::new();
    install_with_providers_and_metrics(
        &mut builder,
        SkillProviders::new().with_host_provider(provider),
        Some(metrics.clone()),
        skills_extension_config,
    );
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let mut config = default_config();
    config.include_instructions = false;
    config.shadow_selection_enabled = true;
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &SessionSource::Cli,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;

    for (turn_id, text) in [("turn-1", "Fix lint errors."), ("turn-2", "continue")] {
        let turn_store = ExtensionData::new(turn_id);
        let fragments = registry.turn_input_contributors()[0]
            .contribute(
                TurnInputContext {
                    turn_id: turn_id.to_string(),
                    user_input: vec![UserInput::Text {
                        text: text.to_string(),
                        text_elements: Vec::new(),
                    }],
                    environments: vec![codex_extension_api::TurnInputEnvironment {
                        environment_id: "test".to_string(),
                        cwd: PathUri::from_host_native_path(
                            std::env::current_dir().expect("test cwd"),
                        )
                        .expect("absolute cwd"),
                        is_primary: true,
                        fs: &FileSystemEnvironmentAccessor::unrestricted(&LOCAL_FS),
                    }],
                },
                /*extension_metrics*/ None,
                &session_store,
                &thread_store,
                &turn_store,
            )
            .await;
        assert!(fragments.is_empty());
        registry.skill_invocation_contributors()[0]
            .on_skill_invocation(SkillInvocationInput {
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
                turn_id,
                skill_resource: "lint-fix/SKILL.md",
                kind: SkillInvocationKind::Implicit,
            })
            .await;
    }

    let snapshots = shadow_task_context_tests::collect_shadow_observations(
        &metrics,
        "codex.skills.shadow_selection.invocation",
        /*expected*/ 24,
    )
    .await?;
    let selector_hits = snapshots
        .iter()
        .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|metric| metric.name() == "codex.skills.shadow_selection.invocation")
        .flat_map(|metric| match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum.data_points(),
            data => panic!("unexpected shadow invocation metric data: {data:?}"),
        })
        .filter_map(|point| {
            let method = point
                .attributes()
                .find(|attribute| attribute.key.as_str() == "method")?
                .value
                .as_str();
            if !matches!(
                method.as_ref(),
                "lru_v1"
                    | "lru_plus_lexical_v1"
                    | "lru_plus_character_routing_v1"
                    | "lru_plus_lexical_character_routing_v1"
            ) {
                return None;
            }
            let hit = point
                .attributes()
                .find(|attribute| attribute.key.as_str() == "hit")?
                .value
                .as_str()
                .to_string();
            Some((method.to_string(), hit, point.value()))
        })
        .fold(
            std::collections::BTreeMap::new(),
            |mut totals, (method, hit, count)| {
                *totals.entry((method, hit)).or_insert(0) += count;
                totals
            },
        )
        .into_iter()
        .map(|((method, hit), count)| (method, hit, count))
        .collect::<Vec<_>>();

    assert_eq!(
        vec![
            (
                "lru_plus_character_routing_v1".to_string(),
                "true".to_string(),
                2,
            ),
            (
                "lru_plus_lexical_character_routing_v1".to_string(),
                "true".to_string(),
                2,
            ),
            ("lru_plus_lexical_v1".to_string(), "true".to_string(), 2),
            ("lru_v1".to_string(), "false".to_string(), 1),
            ("lru_v1".to_string(), "true".to_string(), 1),
        ],
        selector_hits
    );
    Ok(())
}

#[tokio::test]
async fn cloud_discovery_is_gated_and_reports_failures() -> TestResult {
    for cloud_skill_enabled in [false, true] {
        let list_calls = Arc::new(AtomicUsize::new(0));
        let providers = SkillProviders::new().with_cloud_provider(Arc::new(StaticSkillProvider {
            catalog: SkillCatalog::default(),
            read_requests: Arc::new(Mutex::new(Vec::new())),
            list_calls: Some(Arc::clone(&list_calls)),
            fail_first_list: true,
        }));
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut builder =
            ExtensionRegistryBuilder::with_event_sink(Arc::new(ChannelEventSink(event_tx)));
        install_with_providers(&mut builder, providers, skills_extension_config);
        let registry = builder.build();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        let session_source = SessionSource::Cli;
        let config = TestConfig {
            cloud_skill_enabled,
            ..default_config()
        };
        registry.thread_lifecycle_contributors()[0]
            .on_thread_start(ThreadStartInput {
                config: &config,
                session_source: &session_source,
                persistent_thread_state_available: true,
                environments: &[],
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &session_store,
                thread_store: &thread_store,
            })
            .await;

        assert_eq!(
            registry
                .turn_lifecycle_contributors()
                .iter()
                .map(|contributor| contributor.turn_start_phase(&thread_store))
                .collect::<Vec<_>>(),
            vec![
                codex_extension_api::TurnStartPhase::BeforeTaskRegistration,
                if cloud_skill_enabled {
                    codex_extension_api::TurnStartPhase::RegularTaskStart
                } else {
                    codex_extension_api::TurnStartPhase::BeforeTaskRegistration
                },
            ]
        );
        assert_eq!(
            registry
                .turn_lifecycle_contributors()
                .iter()
                .map(|contributor| contributor.requires_mcp_runtime(&thread_store))
                .collect::<Vec<_>>(),
            vec![false, cloud_skill_enabled]
        );
        start_registered_turn(&registry, &session_store, &thread_store, "turn-1").await;
        assert_eq!(
            list_calls.load(Ordering::Relaxed),
            usize::from(cloud_skill_enabled)
        );
        if !cloud_skill_enabled {
            assert!(event_rx.try_recv().is_err());
            let tools = registry.tool_contributors()[0].tools(&session_store, &thread_store);
            assert!(tools.iter().all(|tool| tool.tool_name().name != "list"));
            continue;
        }
        let warning = event_rx.try_recv()?.into_warning();
        assert_eq!(warning.thread_id, thread_store.level_id());
        assert_eq!(warning.turn_id.as_deref(), Some("turn-1"));
        assert!(warning.message.contains("temporary cloud failure"));
    }
    Ok(())
}

#[tokio::test]
async fn root_qualified_locator_selects_only_the_matching_executor_skill() -> TestResult {
    let read_requests = Arc::new(Mutex::new(Vec::new()));
    let root_a_locator = "skill://root-a/shared/lint-fix/SKILL.md";
    let root_b_locator = "skill://root-b/shared/lint-fix/SKILL.md";
    let skill_path = PathUri::parse("file:///shared/lint-fix/SKILL.md")?;
    let executor_provider = Arc::new(StaticSkillProvider {
        catalog: SkillCatalog {
            entries: [("root-a", root_a_locator), ("root-b", root_b_locator)]
                .into_iter()
                .map(|(root_id, locator)| {
                    SkillCatalogEntry::new(
                        SkillPackageId(locator.to_string()),
                        SkillAuthority::new(SkillSourceKind::Executor, root_id),
                        "lint-fix",
                        "Fix lint errors.",
                        SkillResourceId::environment(locator, "env-1", skill_path.clone()),
                    )
                    .with_display_path(locator)
                })
                .collect(),
            warnings: Vec::new(),
        },
        read_requests: Arc::clone(&read_requests),
        list_calls: None,
        fail_first_list: false,
    });
    let providers = SkillProviders::new().with_executor_provider(executor_provider);
    let mut builder = ExtensionRegistryBuilder::new();
    install_with_providers(&mut builder, providers, skills_extension_config);
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let selected_roots = [("root-a", "/skills/root-a"), ("root-b", "/skills/root-b")]
        .into_iter()
        .map(|(id, path)| SelectedCapabilityRoot {
            id: id.to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: "env-1".to_string(),
                path: PathUri::parse(&format!("file://{path}")).expect("skill root URI"),
            },
        })
        .collect::<Vec<_>>();
    let session_source = SessionSource::Cli;
    let config = default_config();
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &session_source,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;

    let turn_store = ExtensionData::new("turn-1");
    registry.context_contributors()[0]
        .contribute_world_state(WorldStateContributionInput {
            previous_world_state: None,
            model_info: &catalog_model_info(),
            thread_id: codex_protocol::ThreadId::new(),
            turn_id: "turn-1",
            environments: &[TurnEnvironmentSelection {
                selected_capability_roots: Default::default(),
                environment_id: "env-1".to_string(),
                cwd: PathUri::parse("file:///workspace").expect("cwd URI"),
                workspace_roots: Vec::new(),
                config: EnvironmentConfigState::FromThread,
            }],
            ready_selected_capability_roots: &selected_roots,
            executor_capability_discovery: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: &turn_store,
            step_store: &turn_store,
        })
        .await;
    let fragments = registry.turn_input_contributors()[0]
        .contribute(
            TurnInputContext {
                turn_id: "turn-1".to_string(),
                user_input: vec![UserInput::Mention {
                    name: "lint-fix".to_string(),
                    path: root_b_locator.to_string(),
                }],
                environments: vec![codex_extension_api::TurnInputEnvironment {
                    environment_id: "env-1".to_string(),
                    cwd: PathUri::parse("file:///workspace")?,
                    is_primary: true,
                    fs: &FileSystemEnvironmentAccessor::unrestricted(&LOCAL_FS),
                }],
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &turn_store,
        )
        .await;

    assert_eq!(1, fragments.len());
    assert!(fragments[0].render().contains(root_b_locator));
    assert_eq!(
        vec![(
            SkillAuthority::new(SkillSourceKind::Executor, "root-b"),
            SkillPackageId(root_b_locator.to_string()),
            SkillResourceId::environment(root_b_locator, "env-1", skill_path),
        )],
        read_request_keys(&read_requests)
    );

    Ok(())
}

#[tokio::test]
async fn prompt_hidden_skill_can_still_be_invoked() -> TestResult {
    let read_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(StaticSkillProvider {
        catalog: SkillCatalog {
            entries: vec![
                test_entry(
                    SkillSourceKind::Host,
                    "host",
                    "host/visible-skill",
                    "visible-skill/SKILL.md",
                ),
                test_entry(
                    SkillSourceKind::Host,
                    "host",
                    "host/hidden-skill",
                    "hidden-skill/SKILL.md",
                )
                .hidden_from_prompt(),
            ],
            warnings: Vec::new(),
        },
        read_requests: Arc::clone(&read_requests),
        list_calls: None,
        fail_first_list: false,
    });
    let providers = SkillProviders::new().with_host_provider(provider);
    let mut builder = ExtensionRegistryBuilder::new();
    install_with_providers(&mut builder, providers, skills_extension_config);
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let session_source = SessionSource::Cli;
    let config = default_config();
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &session_source,
            persistent_thread_state_available: true,
            environments: &[],
            mcp_resource_client: None,
            extension_metrics: None,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;

    let fragments = registry.turn_input_contributors()[0]
        .contribute(
            TurnInputContext {
                turn_id: "turn-1".to_string(),
                user_input: vec![UserInput::Text {
                    text: "$hidden-skill".to_string(),
                    text_elements: Vec::new(),
                }],
                environments: Vec::new(),
            },
            /*extension_metrics*/ None,
            &session_store,
            &thread_store,
            &ExtensionData::new("turn-1"),
        )
        .await;

    assert_eq!(1, fragments.len());
    assert!(fragments[0].render().contains("<name>hidden-skill</name>"));
    assert_eq!(
        vec![(
            SkillAuthority::new(SkillSourceKind::Host, "host"),
            SkillPackageId("host/hidden-skill".to_string()),
            SkillResourceId::new("hidden-skill/SKILL.md"),
        )],
        read_request_keys(&read_requests)
    );

    Ok(())
}

#[derive(Clone)]
struct StaticSkillProvider {
    catalog: SkillCatalog,
    read_requests: Arc<Mutex<Vec<(SkillAuthority, SkillPackageId, SkillResourceId)>>>,
    list_calls: Option<Arc<AtomicUsize>>,
    fail_first_list: bool,
}

#[derive(Debug)]
enum CapturedExtensionEvent {
    Event(Box<Event>),
    Warning(ExtensionWarning),
}

impl CapturedExtensionEvent {
    fn into_warning(self) -> ExtensionWarning {
        match self {
            Self::Warning(warning) => warning,
            Self::Event(event) => panic!("expected extension warning, got {event:?}"),
        }
    }
}

struct ChannelEventSink(std::sync::mpsc::Sender<CapturedExtensionEvent>);

impl ExtensionEventSink for ChannelEventSink {
    fn emit(&self, event: Event) {
        let _ = self.0.send(CapturedExtensionEvent::Event(Box::new(event)));
    }

    fn emit_warning(&self, warning: ExtensionWarning) {
        let _ = self.0.send(CapturedExtensionEvent::Warning(warning));
    }
}

impl SkillProvider for StaticSkillProvider {
    fn list(&self, _query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        let list_call = self
            .list_calls
            .as_ref()
            .map(|list_calls| list_calls.fetch_add(1, Ordering::Relaxed));
        let fail = self.fail_first_list && list_call == Some(0);
        let catalog = self.catalog.clone();
        Box::pin(async move {
            if fail {
                Err(SkillProviderError::new("temporary cloud failure"))
            } else {
                Ok(catalog)
            }
        })
    }

    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        let read_requests = Arc::clone(&self.read_requests);
        Box::pin(async move {
            read_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    request.authority.clone(),
                    request.package.clone(),
                    request.resource.clone(),
                ));
            Ok(SkillReadResult {
                resource: request.resource,
                contents: "# Lint Fix\n\nRun the formatter.".to_string(),
            })
        })
    }

    fn search(&self, _request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult> {
        Box::pin(async { Ok(SkillSearchResult::default()) })
    }
}

fn test_entry(
    kind: SkillSourceKind,
    authority_id: &str,
    package_id: &str,
    main_prompt: &str,
) -> SkillCatalogEntry {
    let name = package_id.rsplit('/').next().unwrap_or(package_id);
    SkillCatalogEntry::new(
        SkillPackageId(package_id.to_string()),
        SkillAuthority::new(kind, authority_id),
        name,
        "Fix lint errors.",
        SkillResourceId::new(main_prompt),
    )
    .with_display_path(format!("skill://{package_id}/SKILL.md"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestConfig {
    include_instructions: bool,
    bundled_skills_enabled: bool,
    cloud_skill_enabled: bool,
    shadow_selection_enabled: bool,
}

fn default_config() -> TestConfig {
    TestConfig {
        include_instructions: true,
        bundled_skills_enabled: true,
        cloud_skill_enabled: true,
        shadow_selection_enabled: false,
    }
}

fn skills_extension_config(config: &TestConfig) -> SkillsExtensionConfig {
    SkillsExtensionConfig {
        include_instructions: config.include_instructions,
        max_context_tokens: None,
        bundled_skills_enabled: config.bundled_skills_enabled,
        cloud_skill_enabled: config.cloud_skill_enabled,
        shadow_selection_enabled: config.shadow_selection_enabled,
    }
}

fn test_codex_home() -> PathBuf {
    let id = NEXT_CODEX_HOME_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "codex-skills-extension-test-{}-{id}",
        std::process::id(),
    ))
}

fn read_request_keys(
    requests: &Mutex<Vec<(SkillAuthority, SkillPackageId, SkillResourceId)>>,
) -> Vec<(SkillAuthority, SkillPackageId, SkillResourceId)> {
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
