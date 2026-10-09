use anyhow::Result;
use predicates::str::contains;
use tempfile::TempDir;

fn command(home: &TempDir) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .env("CODEX_HOME", home.path())
        .env("OPENAI_API_KEY", "test-api-key")
        .env_remove("CODEX_API_KEY")
        .current_dir(home.path());
    Ok(command)
}

#[test]
fn settings_recover_api_key_startup_without_thread_or_deno_and_preserve_options() -> Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("config.toml");
    let original = "[features.code_mode]\ndeno_program = '/missing/deno'\nnotebook_profile = 'keep'\nnotebook_max_heap_mib = 2048\n[features.multi_agent_v2]\nwait_agent_enabled = false\n";
    std::fs::write(&path, original)?;
    command(&home)?
        .args(["settings"])
        .assert()
        .success()
        .stdout(contains("context = notes"))
        .stdout(contains("Choices: off, v8, notebook"));
    assert_eq!(std::fs::read_to_string(&path)?, original);
    for args in [
        ["settings", "set", "context", "compaction"],
        ["settings", "set", "code-mode", "v8"],
        ["settings", "set", "multi-agent", "off"],
    ] {
        command(&home)?
            .args(args)
            .assert()
            .success()
            .stdout(contains("Running threads are unchanged"));
    }
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
    assert_eq!(config["context_strategy"].as_str(), Some("compaction"));
    assert_eq!(
        config["features"]["code_mode"]["runtime"].as_str(),
        Some("v8")
    );
    assert_eq!(
        config["features"]["code_mode"]["notebook_profile"].as_str(),
        Some("keep")
    );
    assert_eq!(
        config["features"]["code_mode"]["notebook_max_heap_mib"].as_integer(),
        Some(2048)
    );
    assert_eq!(
        config["features"]["multi_agent_v2"]["wait_agent_enabled"].as_bool(),
        Some(false)
    );
    assert_eq!(
        config["features"]["multi_agent_v2"]["enabled"].as_bool(),
        Some(false)
    );
    assert!(!home.path().join("sessions").exists());
    assert!(!home.path().join("notebook").exists());
    Ok(())
}

#[test]
fn notes_idle_rollover_accepts_only_off_or_25_and_preserves_other_settings() -> Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("config.toml");
    let original = "context_strategy = 'notes'\n";
    std::fs::write(&path, original)?;
    command(&home)?
        .args(["settings"])
        .assert()
        .success()
        .stdout(contains("notes-idle-rollover = off\n  Choices: off, 25\n"));
    for choice in ["15", "30", "60"] {
        command(&home)?
            .args(["settings", "set", "notes-idle-rollover", choice])
            .assert()
            .failure()
            .stderr(contains("Choose: off, 25"));
        assert_eq!(std::fs::read_to_string(&path)?, original);
    }
    for (choice, expected) in [("25", Some(25)), ("off", None)] {
        command(&home)?
            .args(["settings", "set", "notes-idle-rollover", choice])
            .assert()
            .success();
        let config: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(
            config
                .get("context_idle_rollover_minutes")
                .and_then(toml::Value::as_integer),
            expected
        );
        assert_eq!(config["context_strategy"].as_str(), Some("notes"));
    }
    Ok(())
}

#[test]
fn settings_reject_invalid_choices_and_report_overridden_saves() -> Result<()> {
    let home = TempDir::new()?;
    command(&home)?
        .args(["settings", "set", "context", "none"])
        .assert()
        .failure()
        .stderr(contains("Invalid choice"));
    assert!(!home.path().join("config.toml").exists());
    command(&home)?
        .args([
            "-c",
            "context_strategy='notes'",
            "settings",
            "set",
            "context",
            "compaction",
        ])
        .assert()
        .failure()
        .stderr(contains("saved, but overridden"));
    let config: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.path().join("config.toml"))?)?;
    assert_eq!(config["context_strategy"].as_str(), Some("compaction"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_reject_managed_feature_denial_before_writing() -> Result<()> {
    use app_test_support::ChatGptAuthFixture;
    use app_test_support::write_chatgpt_auth;
    use codex_config::types::AuthCredentialsStoreMode;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    let server = MockServer::start().await;
    let home = TempDir::new()?;
    let config = format!(
        "cli_auth_credentials_store = 'file'\nchatgpt_base_url = '{}/backend-api'\n[features.code_mode]\nenabled = true\nnotebook_profile = 'keep'\n",
        server.uri()
    );
    std::fs::write(home.path().join("config.toml"), &config)?;
    write_chatgpt_auth(
        home.path(),
        ChatGptAuthFixture::new("test-token")
            .account_id("workspace-123")
            .chatgpt_account_id("workspace-123")
            .chatgpt_user_id("user-123")
            .plan_type("enterprise"),
        AuthCredentialsStoreMode::File,
    )?;
    Mock::given(method("GET")).and(path("/backend-api/wham/config/bundle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"requirements_toml": {"enterprise_managed": [{"id": "code-mode-required", "name": "Required Code Mode", "contents": "[features]\ncode_mode = true\n"}]}})))
        .expect(1).mount(&server).await;
    command(&home)?
        .env_remove("OPENAI_API_KEY")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["settings", "set", "code-mode", "off"])
        .assert()
        .failure()
        .stderr(contains("managed feature requirements"));
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml"))?,
        config
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_reject_managed_context_conflicts_before_writing() -> Result<()> {
    use app_test_support::ChatGptAuthFixture;
    use app_test_support::write_chatgpt_auth;
    use codex_config::types::AuthCredentialsStoreMode;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    for feature in ["token_budget", "context_management"] {
        for (choice, required) in [("compaction", true), ("notes", false)] {
            let server = MockServer::start().await;
            let home = TempDir::new()?;
            let config = format!(
                "cli_auth_credentials_store = 'file'\nchatgpt_base_url = '{}/backend-api'\n[features.code_mode]\nnotebook_profile = 'keep'\n",
                server.uri()
            );
            std::fs::write(home.path().join("config.toml"), &config)?;
            write_chatgpt_auth(
                home.path(),
                ChatGptAuthFixture::new("test-token")
                    .account_id("workspace-123")
                    .chatgpt_account_id("workspace-123")
                    .chatgpt_user_id("user-123")
                    .plan_type("enterprise"),
                AuthCredentialsStoreMode::File,
            )?;
            Mock::given(method("GET")).and(path("/backend-api/wham/config/bundle"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"requirements_toml": {"enterprise_managed": [{"id": "context-required", "name": "Required context policy", "contents": format!("[features]\n{feature} = {required}\n")}]}})))
                .expect(1).mount(&server).await;
            command(&home)?
                .env_remove("OPENAI_API_KEY")
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(["settings", "set", "context", choice])
                .assert()
                .failure()
                .stderr(contains("conflicts with managed requirement"))
                .stderr(contains(feature));
            assert_eq!(
                std::fs::read_to_string(home.path().join("config.toml"))?,
                config
            );
            assert!(!home.path().join("sessions").exists());
            assert!(!home.path().join("notebook").exists());
        }
    }
    Ok(())
}
