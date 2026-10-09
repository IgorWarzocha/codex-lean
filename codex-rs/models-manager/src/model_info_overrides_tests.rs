use crate::ModelsManagerConfig;
use crate::manager::ModelsManager;
use codex_protocol::openai_models::MultiAgentModeMessages;
use codex_protocol::openai_models::TruncationPolicyConfig;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::TestModelsEndpoint;
use super::openai_manager_for_tests;

#[tokio::test]
async fn default_catalog_compacts_remote_workflow_but_explicit_catalogs_keep_it() {
    let home = TempDir::new().expect("create temp dir");
    let mut model = super::load_remote_models_from_file()
        .expect("bundled models")
        .into_iter()
        .find(|model| model.slug == "gpt-6-luna")
        .expect("model with workflow messages");
    // Exercise remote normalization independently of the compact bundled catalog.
    let messages = model.model_messages.as_mut().unwrap();
    messages.persistent_instructions = Some("Remote persistence tutorial. ".repeat(64));
    messages.collaboration_modes.as_mut().unwrap().default =
        Some("Remote default-mode tutorial.".to_string());
    let agents = messages.multi_agent.as_mut().unwrap();
    agents.role.as_mut().unwrap().root = Some("Remote root-role tutorial.".to_string());
    agents.mode = Some(MultiAgentModeMessages {
        explicit: Some("Remote explicit-delegation tutorial.".to_string()),
        proactive: Some("Remote proactive-delegation tutorial.".to_string()),
        hint_text: Some("Retain this specific mode hint.".to_string()),
    });
    messages.token_budget.as_mut().unwrap().guidance_message =
        "Remote context-budget tutorial. ".repeat(32);
    let catalog_messages = model.model_messages.as_ref().expect("catalog messages");
    let mut expected = catalog_messages.clone();
    codex_prompts::apply_default_catalog_workflow(&mut expected);
    let defaults = codex_prompts::ResolvedModelMessages::bundled();
    assert_eq!(
        expected.persistent_instructions.as_deref(),
        Some(defaults.persistent_instructions())
    );
    assert!(
        expected.persistent_instructions.as_ref().unwrap().len()
            < catalog_messages
                .persistent_instructions
                .as_ref()
                .unwrap()
                .len()
    );
    assert_eq!(
        expected
            .collaboration_modes
            .as_ref()
            .unwrap()
            .default
            .as_deref(),
        Some(defaults.collaboration_modes().default.text())
    );
    assert_eq!(
        expected
            .multi_agent
            .as_ref()
            .unwrap()
            .role
            .as_ref()
            .unwrap()
            .root
            .as_deref(),
        Some(defaults.multi_agent().root.text())
    );
    let mode = expected
        .multi_agent
        .as_ref()
        .unwrap()
        .mode
        .as_ref()
        .unwrap();
    assert_eq!(
        mode.explicit.as_deref(),
        Some(defaults.multi_agent().explicit.text())
    );
    assert_eq!(
        mode.proactive.as_deref(),
        Some(defaults.multi_agent().proactive.text())
    );
    assert_eq!(
        mode.hint_text.as_deref(),
        Some("Retain this specific mode hint.")
    );
    let budget = expected.token_budget.as_ref().unwrap();
    let original_budget = catalog_messages.token_budget.as_ref().unwrap();
    assert!(budget.guidance_message.len() < original_budget.guidance_message.len());
    assert!(budget.reminder_message_template.contains("{n_remaining}"));
    let mut preserved_budget = budget.clone();
    preserved_budget.guidance_message = original_budget.guidance_message.clone();
    preserved_budget.reminder_message_template = original_budget.reminder_message_template.clone();
    preserved_budget.auto_compact_fallback_prompt =
        original_budget.auto_compact_fallback_prompt.clone();
    assert_eq!(preserved_budget, *original_budget);
    let mut preserved_messages = expected.clone();
    preserved_messages.persistent_instructions = catalog_messages.persistent_instructions.clone();
    preserved_messages.collaboration_modes = catalog_messages.collaboration_modes.clone();
    preserved_messages.multi_agent = catalog_messages.multi_agent.clone();
    preserved_messages.token_budget = catalog_messages.token_budget.clone();
    assert_eq!(preserved_messages, *catalog_messages);

    for explicit_provider in [false, true] {
        let endpoint = TestModelsEndpoint::with_command_auth(vec![Ok(vec![model.clone()])]);
        let manager = openai_manager_for_tests(home.path().to_path_buf(), endpoint);
        let manager = if explicit_provider {
            manager.with_provider_catalog()
        } else {
            manager
        };
        manager
            .refresh_available_models(
                super::RefreshStrategy::Online,
                &super::DEFAULT_HTTP_CLIENT_FACTORY,
            )
            .await
            .expect("refresh fixture");
        // Sparse per-turn config must not lose the manager's catalog authority.
        for base in [None, Some("custom base instructions".to_string())] {
            let config = ModelsManagerConfig {
                base_instructions: base.clone(),
                ..Default::default()
            };
            let actual = manager.get_model_info(&model.slug, &config).await;
            let mut selected = if explicit_provider {
                catalog_messages.clone()
            } else {
                expected.clone()
            };
            selected.instructions_template = base.or_else(|| {
                if explicit_provider {
                    catalog_messages.instructions_template.clone()
                } else {
                    Some(crate::model_info::BASE_INSTRUCTIONS.to_string())
                }
            });
            assert_eq!(actual.model_messages, Some(selected));
        }
    }
    let static_manager = super::static_manager_for_tests(super::ModelsResponse {
        models: vec![model.clone()],
    });
    assert_eq!(
        static_manager
            .get_model_info(&model.slug, &ModelsManagerConfig::default())
            .await
            .model_messages,
        model.model_messages
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_model_info_without_tool_output_override() {
    let codex_home = TempDir::new().expect("create temp dir");
    let config = ModelsManagerConfig::default();
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );

    let model_info = manager.get_model_info("unknown-model", &config).await;

    assert_eq!(
        model_info.truncation_policy,
        TruncationPolicyConfig::bytes(/*limit*/ 10_000)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_model_info_with_tool_output_override() {
    let codex_home = TempDir::new().expect("create temp dir");
    let config = ModelsManagerConfig {
        tool_output_token_limit: Some(123),
        ..Default::default()
    };
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );

    let model_info = manager.get_model_info("gpt-5.5", &config).await;

    assert_eq!(
        model_info.truncation_policy,
        TruncationPolicyConfig::tokens(/*limit*/ 123)
    );
}
