//! Native sampling regressions for instruction discovery, not shell-result augmentation.

use super::*;
use codex_config::types::ContextStrategy;
use codex_features::CodeModeRuntime;
use codex_login::CodexAuth;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use pretty_assertions::assert_eq;

fn discovery_call(id: &str, command: &str, runtime: Option<CodeModeRuntime>) -> serde_json::Value {
    let args = json!({"cmd": command, "yield_time_ms": 1000}).to_string();
    if runtime.is_some() {
        ev_custom_tool_call(
            id,
            "exec",
            &format!("text(await tools.exec_command({args}));"),
        )
    } else {
        ev_function_call(id, "exec_command", &args)
    }
}

fn configure_runtime(config: &mut codex_core::config::Config, runtime: Option<CodeModeRuntime>) {
    config.active_project.trust_level = Some(TrustLevel::Trusted);
    if let Some(runtime) = runtime {
        config.code_mode.runtime = runtime;
        config.code_mode.deno_program = Some(
            std::env::var_os("DENO_PROGRAM")
                .map(Into::into)
                .unwrap_or_else(|| "deno".into()),
        );
        config
            .features
            .enable(Feature::CodeModeOnly)
            .expect("configure test feature flags");
    } else {
        config
            .features
            .disable(Feature::CodeModeOnly)
            .expect("configure test feature flags");
        config
            .features
            .disable(Feature::CodeMode)
            .expect("configure test feature flags");
    }
}

async fn setup_workspace(
    cwd: AbsolutePathBuf,
    fs: Arc<dyn codex_exec_server::ExecutorFileSystem>,
) -> Result<()> {
    fs.create_directory(
        &executor_path_uri(cwd.join("scoped/deep"))?,
        CreateDirectoryOptions {
            recursive: true,
            follow_symlinks: true,
        },
        None,
    )
    .await?;
    fs.create_directory(
        &executor_path_uri(cwd.join("unreached"))?,
        CreateDirectoryOptions {
            recursive: true,
            follow_symlinks: true,
        },
        None,
    )
    .await?;
    for (path, contents) in [
        (".git", ""),
        ("AGENTS.md", "startup-root-policy"),
        ("scoped/AGENTS.md", "parent-scoped-policy"),
        ("scoped/deep/AGENTS.md", "deep-scoped-policy"),
        ("scoped/deep/item.txt", "needle"),
        ("unreached/AGENTS.md", "must-not-preload-policy"),
    ] {
        fs.write_file(
            &executor_path_uri(cwd.join(path))?,
            contents.as_bytes().to_vec(),
            Default::default(),
            None,
        )
        .await?;
    }
    Ok(())
}

#[test_case::test_case(None; "direct")]
#[test_case::test_case(Some(CodeModeRuntime::V8); "code")]
#[test_case::test_case(Some(CodeModeRuntime::Notebook); "notebook")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_discovery_precedes_sampling_and_survives_resume(
    runtime: Option<CodeModeRuntime>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = start_mock_server().await;
    let search = match runtime {
        None => "cd scoped && rg -n needle .",
        Some(CodeModeRuntime::V8) => "cd scoped && rg -l needle .",
        Some(CodeModeRuntime::Notebook) => "cd scoped && rg --files-with-matches needle .",
    };
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("discover"),
                discovery_call("discover", search, runtime),
                ev_completed("discover"),
            ]),
            sse(vec![
                ev_response_created("read"),
                discovery_call("read", "cat scoped/deep/item.txt", runtime),
                ev_completed("read"),
            ]),
            responses::sse_completed("done"),
            responses::sse_completed("resumed"),
            responses::sse_completed("changed"),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_config(move |config| configure_runtime(config, runtime))
        .with_workspace_setup(setup_workspace);
    let test = builder.build(&server).await?;
    test.submit_turn("discover and read the nested file")
        .await?;
    let captured = requests.requests();
    assert_eq!(captured.len(), 3);
    let first = instruction_fragments(&captured[0]);
    assert_eq!(first.len(), 1);
    assert!(first[0].contains("startup-root-policy"));
    let following = instruction_fragments(&captured[1]);
    assert_eq!(
        following.len(),
        2,
        "discovered instructions must enter the next sampling request"
    );
    assert_eq!(
        following[0], first[0],
        "the cached root prefix must remain unchanged"
    );
    assert!(following[1].contains("parent-scoped-policy"));
    assert!(following[1].contains("deep-scoped-policy"));
    assert!(following[1].find("parent-scoped-policy") < following[1].find("deep-scoped-policy"));
    assert!(following[1].contains("apply only to paths beneath"));
    assert!(!following[1].contains("must-not-preload-policy"));
    assert_eq!(
        instruction_fragments(&captured[2]),
        following,
        "re-reading unchanged guidance must not append it again"
    );

    let rollout = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout");
    test.codex.shutdown_and_wait().await?;
    let original_cwd = test.config.cwd.clone();
    let mut resume_builder = test_codex().with_config(move |config| {
        configure_runtime(config, runtime);
        config.cwd = original_cwd;
    });
    let resumed = resume_builder
        .resume(&server, Arc::clone(&test.home), rollout)
        .await?;
    resumed
        .submit_turn("continue with the same guidance")
        .await?;
    assert_eq!(
        instruction_fragments(&requests.requests()[3]),
        following,
        "resume must retain visible guidance without duplication"
    );
    resumed
        .fs()
        .write_file(
            &executor_path_uri(test.config.cwd.join("scoped/deep/AGENTS.md"))?,
            b"changed-deep-policy".to_vec(),
            Default::default(),
            None,
        )
        .await?;
    resumed
        .submit_turn("continue after the policy changed")
        .await?;
    let changed = instruction_fragments(&requests.requests()[4]);
    assert_eq!(changed.len(), 3);
    assert!(changed[2].contains("changed-deep-policy"));
    assert!(
        !changed[2].contains("parent-scoped-policy"),
        "unchanged parent content must not be redelivered"
    );
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_guidance_is_restored_in_a_notes_window() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("read"),
                discovery_call("read", "cat scoped/deep/item.txt", None),
                ev_completed("read"),
            ]),
            sse(vec![
                ev_response_created("rollover"),
                ev_function_call("rollover", "new_context", "{}"),
                ev_completed("rollover"),
            ]),
            responses::sse_completed("done"),
        ],
    )
    .await;
    let backend_url = format!("{}/backend-api/codex", server.uri());
    let test = test_codex()
        .with_context_strategy(ContextStrategy::Notes)
        .with_auth(CodexAuth::from_external_chatgpt_tokens(
            "header.e30.signature",
            "account-123",
            Some("plus"),
        )?)
        .with_config(move |config| {
            configure_runtime(config, None);
            config.model_provider.base_url = Some(backend_url);
        })
        .with_workspace_setup(setup_workspace)
        .build(&server)
        .await?;
    test.submit_turn("read the file and start a new notes window")
        .await?;
    let captured = requests.requests();
    assert_eq!(captured.len(), 3);
    let restored = instruction_fragments(&captured[2]);
    assert_eq!(restored.len(), 2);
    assert!(restored[0].contains("startup-root-policy"));
    assert!(restored[1].contains("parent-scoped-policy"));
    assert!(restored[1].contains("deep-scoped-policy"));
    assert!(
        !captured[2]
            .body_json()
            .to_string()
            .contains("read the file and start a new notes window"),
        "old history must actually be gone"
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_listing_scope_and_native_patch_refresh_are_kept_separate() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = start_mock_server().await;
    let patch = "*** Begin Patch\n*** Update File: scoped/deep/AGENTS.md\n@@\n-deep-scoped-policy\n+patched-deep-policy\n*** End Patch";
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("list"),
                discovery_call("list", "ls scoped", None),
                ev_completed("list"),
            ]),
            sse(vec![
                ev_response_created("deep"),
                discovery_call("deep", "ls scoped/deep", None),
                ev_completed("deep"),
            ]),
            sse(vec![
                ev_response_created("patch"),
                ev_custom_tool_call("patch", "apply_patch", patch),
                ev_completed("patch"),
            ]),
            responses::sse_completed("done"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| configure_runtime(config, None))
        .with_workspace_setup(setup_workspace)
        .build(&server)
        .await?;
    test.submit_turn("list directories then update their policy")
        .await?;
    let captured = requests.requests();
    assert_eq!(captured.len(), 4);
    let listed = instruction_fragments(&captured[1]);
    assert_eq!(listed.len(), 2);
    assert!(listed[1].contains("parent-scoped-policy"));
    assert!(
        !listed[1].contains("deep-scoped-policy"),
        "listing children must not preload their policies"
    );
    let reached = instruction_fragments(&captured[2]);
    assert_eq!(reached.len(), 3);
    assert!(
        reached[2].contains("deep-scoped-policy"),
        "the full queried operand must resolve beneath scoped/deep"
    );
    let changed = instruction_fragments(&captured[3]);
    assert_eq!(changed.len(), 4);
    assert!(
        changed[3].contains("patched-deep-policy"),
        "native Custom apply_patch must invalidate reached instructions before same-turn sampling"
    );
    assert!(!changed[3].contains("parent-scoped-policy"));
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
