//! Core cloud-skill behavior exercised without the concrete MCP-backed provider.

use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;
use tokio::sync::Mutex;

#[path = "yielded_skill_tests.rs"]
mod yielded_skill_tests;

struct FakeCloudSkillProvider {
    catalog: SkillCatalog,
    resources: std::collections::HashMap<String, String>,
    reads: Mutex<Vec<String>>,
}

#[test_case(false; "visible")]
#[test_case(true; "explicit only")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progressive_cloud_reads_work_without_executor_and_through_code_mode(
    hidden: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PACKAGE: &str = "skill://demo/deploy";
    const MAIN: &str = "skill://demo/deploy/SKILL.md";
    const REFERENCE: &str = "skill://demo/deploy/references/deploy.md";
    let server = responses::start_mock_server().await;
    let mut entry = SkillCatalogEntry::new(
        SkillPackageId(PACKAGE.to_string()),
        SkillAuthority::new(SkillSourceKind::Cloud, CODEX_APPS_MCP_SERVER_NAME),
        "demo:deploy",
        "Deploy",
        SkillResourceId::new(MAIN),
    );
    if hidden {
        entry = entry.hidden_from_prompt();
    }
    let provider = Arc::new(FakeCloudSkillProvider {
        catalog: SkillCatalog {
            entries: vec![entry],
            warnings: vec![],
        },
        resources: std::collections::HashMap::from([
            (
                MAIN.to_string(),
                "---\nname: deploy\ndescription: Deploy\n---\nDEPLOY_BODY".to_string(),
            ),
            (REFERENCE.to_string(), "DEPLOY_REFERENCE".to_string()),
        ]),
        reads: Mutex::default(),
    });
    let mut extensions = ExtensionRegistryBuilder::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_cloud_provider(provider.clone()),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: true,
            shadow_selection_enabled: false,
        },
    );
    let chatgpt_base_url = server.uri();
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_exec_server_url("none")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.chatgpt_base_url = chatgpt_base_url;
            config.cloud_skill_enabled = true;
            config
                .features
                .enable(Feature::CodeMode)
                .expect("configure test feature flags");
            config
                .features
                .enable(Feature::CodeModeHost)
                .expect("configure test feature flags");
        })
        .with_code_mode_host_program(codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?);
    let test = builder.build_with_auto_env(&server).await?;
    let response = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("tools"),
                responses::ev_custom_tool_call("list", "skills", "list"),
                responses::ev_custom_tool_call(
                    "reference",
                    "skills",
                    &format!("read {PACKAGE} {REFERENCE}"),
                ),
                responses::ev_custom_tool_call(
                    "code",
                    "exec",
                    &format!("text(await tools.skills(\"read {PACKAGE}\"));"),
                ),
                ev_completed("tools"),
            ]),
            sse(vec![
                ev_response_created("again"),
                responses::ev_custom_tool_call("again", "skills", &format!("read {REFERENCE}")),
                ev_completed("again"),
            ]),
            sse(vec![ev_response_created("done"), ev_completed("done")]),
        ],
    )
    .await;
    test.submit_turn(if hidden {
        "Use $demo:deploy."
    } else {
        "Inspect skills."
    })
    .await?;
    let requests = response.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        skill_output(&requests[1], "list"),
        if hidden {
            "No skills available."
        } else {
            "- demo:deploy: Deploy"
        }
    );
    let reference = skill_output(&requests[1], "reference");
    assert!(reference.starts_with("DEPLOY_REFERENCE"));
    assert!(!reference.contains("DEPLOY_BODY"));
    assert!(reference.contains(REFERENCE));
    assert_eq!(skill_output(&requests[2], "again"), reference);
    assert!(
        crate::suite::code_mode::custom_tool_output_last_non_empty_text(&requests[1], "code")
            .expect("Code Mode text")
            .contains("DEPLOY_BODY")
    );
    let reads = provider.reads.lock().await;
    assert_eq!(
        reads
            .iter()
            .filter(|resource| resource.as_str() == MAIN)
            .count(),
        1
    );
    assert_eq!(
        reads
            .iter()
            .filter(|resource| resource.as_str() == REFERENCE)
            .count(),
        1
    );
    assert!(
        !requests[0]
            .message_input_texts("developer")
            .join("\n")
            .contains("- demo:deploy:")
    );
    if hidden {
        assert!(
            requests[0]
                .message_input_texts("user")
                .join("\n")
                .contains("DEPLOY_BODY")
        );
    }
    Ok(())
}

impl SkillProvider for FakeCloudSkillProvider {
    fn list(&self, _query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async { Ok(self.catalog.clone()) })
    }

    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            assert_eq!(
                request.authority,
                SkillAuthority::new(SkillSourceKind::Cloud, CODEX_APPS_MCP_SERVER_NAME)
            );
            assert!(
                self.catalog
                    .entries
                    .iter()
                    .any(|entry| entry.id == request.package)
            );
            self.reads
                .lock()
                .await
                .push(request.resource.as_str().to_string());
            let contents = self
                .resources
                .get(request.resource.as_str())
                .cloned()
                .ok_or_else(|| SkillProviderError::new("unknown fake skill resource"))?;
            Ok(SkillReadResult {
                resource: request.resource,
                contents,
            })
        })
    }

    fn search(&self, _request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult> {
        Box::pin(async { Ok(SkillSearchResult::default()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_cloud_skill_remains_available_without_host_discovery() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const SKILL_ROOT: &str = "skill://plugin_connector_1p_2330815c823c8191941e5dc465bb899f";
    const SKILL_BODY: &str = "CLOUD_SKILL_REMAINS_AVAILABLE_WITHOUT_HOST_DISCOVERY";

    let server = responses::start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut extensions = ExtensionRegistryBuilder::new();
    install_with_providers(
        &mut extensions,
        SkillProviders::new().with_cloud_provider(Arc::new(FakeCloudSkillProvider {
            catalog: SkillCatalog {
                entries: vec![
                    SkillCatalogEntry::new(
                        SkillPackageId(format!("{SKILL_ROOT}/search")),
                        SkillAuthority::new(SkillSourceKind::Cloud, CODEX_APPS_MCP_SERVER_NAME),
                        "demo:search",
                        "Search company knowledge.",
                        SkillResourceId::new(format!("{SKILL_ROOT}/search/SKILL.md")),
                    )
                    .with_display_path(format!("{SKILL_ROOT}/search"))
                    .with_alias_root(SKILL_ROOT),
                ],
                warnings: Vec::new(),
            },
            resources: std::collections::HashMap::from([(
                format!("{SKILL_ROOT}/search/SKILL.md"),
                SKILL_BODY.to_string(),
            )]),
            reads: Mutex::default(),
        })),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            cloud_skill_enabled: true,
            shadow_selection_enabled: false,
        },
    );
    let chatgpt_base_url = server.uri();
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| config.chatgpt_base_url = chatgpt_base_url)
        .with_exec_server_url("none")
        .with_extensions(Arc::new(extensions.build()))
        .with_model_info_override("gpt-5.5", |model_info| {
            model_info.context_window = Some(2_000);
            model_info.max_context_window = None;
        })
        .with_config(|config| {
            config.include_skill_instructions = true;
            config.cloud_skill_enabled = true;
            config
                .features
                .enable(Feature::SkipHostSkillDiscovery)
                .expect("cloud skills must not depend on host discovery");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("Use $demo:search.").await?;

    let request = response.single_request();
    let developer_text = request.message_input_texts("developer").join("\n");
    assert!(!developer_text.contains("### Available skills"));
    let user_text = request.message_input_texts("user").join("\n");
    assert!(
        user_text.contains("<skill>\n<name>demo:search</name>") && user_text.contains(SKILL_BODY),
        "cloud instruction reads must remain available without host discovery: {user_text}"
    );

    Ok(())
}
