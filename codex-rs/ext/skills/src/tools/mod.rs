use std::sync::Arc;

use codex_analytics::AnalyticsEventsClient;
use codex_analytics::InvocationType;
use codex_analytics::SkillInvocation;
use codex_analytics::SkillInvocationLocation;
use codex_analytics::build_track_events_context;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::SelectedPluginSnapshot;
use codex_extension_api::ThreadOriginator;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpResourceClient;
use codex_otel::SessionTelemetry;
use codex_otel::SkillInvocationEvent;
use codex_otel::SkillInvocationType;
use codex_otel::sanitize_metric_tag_value;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::HostSkillsSnapshot;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;
use crate::provider::SkillListQuery;
use crate::provider::attribute_executor_plugins;
use crate::sources::SkillProviders;
use crate::state::SkillsSessionState;
use crate::state::SkillsThreadState;
use crate::telemetry::ActiveSkillTurnMetrics;
use crate::telemetry::SkillTurnMetrics;

mod command;
mod list;
mod read;
mod selection;

pub(crate) const SKILLS_GUIDANCE: &str = "Skills: List once at session start; read always-applicable and task-relevant skills before work";

pub(crate) fn skill_tools(
    providers: SkillProviders,
    session_store: &ExtensionData,
    thread_store: &ExtensionData,
    executor_query: Option<SkillListQuery>,
    selected_plugins: Option<Arc<SelectedPluginSnapshot>>,
    host_snapshot: Option<Arc<HostSkillsSnapshot>>,
) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
    let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
        return Vec::new();
    };
    let cloud_available = providers.has_cloud_provider() && thread_state.cloud_skill_enabled();
    let mcp_resources = session_store
        .get::<SkillsSessionState>()
        .and_then(|state| state.mcp_resources.clone());
    let analytics = SkillAnalytics::from_stores(session_store, thread_store);
    let context = SkillToolContext {
        providers,
        mcp_resources,
        thread_state,
        analytics,
        cloud_available,
        executor_query,
        selected_plugins,
        host_snapshot,
        executor_catalog: Arc::new(OnceCell::new()),
    };
    vec![Arc::new(command::SkillsTool { context })]
}

#[derive(Clone)]
pub(crate) struct SkillAnalytics {
    client: AnalyticsEventsClient,
    telemetry: Option<Arc<SessionTelemetry>>,
    metrics: Option<Arc<dyn ExtensionMetrics>>,
    turn_metrics: Option<Arc<SkillTurnMetrics>>,
    thread_id: String,
    product_client_id: String,
}

impl SkillAnalytics {
    pub(crate) fn from_stores(
        session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Option<Self> {
        let client = session_store.get::<AnalyticsEventsClient>()?;
        let originator = thread_store.get::<ThreadOriginator>()?;

        Some(Self {
            client: client.as_ref().clone(),
            telemetry: session_store.get::<SessionTelemetry>(),
            metrics: session_store
                .get::<SkillsSessionState>()
                .and_then(|state| state.extension_metrics.clone()),
            // Code-mode callbacks retain these tools after another turn becomes active.
            turn_metrics: thread_store
                .get_or_init(ActiveSkillTurnMetrics::default)
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .upgrade(),
            thread_id: thread_store.level_id().to_string(),
            product_client_id: originator.0.clone(),
        })
    }

    pub(crate) fn track_skill_invocation(
        &self,
        skill: &SkillCatalogEntry,
        model: String,
        turn_id: String,
        invocation_type: InvocationType,
    ) {
        if let Some(telemetry) = &self.telemetry {
            telemetry
                .as_ref()
                .clone()
                .with_model(&model, &model)
                .skill_invocation(SkillInvocationEvent {
                    turn_id: &turn_id,
                    skill_name: &skill.name,
                    scope: skill.analytics_scope,
                    plugin_id: skill.plugin_id.as_deref(),
                    invocation_type: match invocation_type {
                        InvocationType::Explicit => SkillInvocationType::Explicit,
                        InvocationType::Implicit => SkillInvocationType::Implicit,
                    },
                });
        }
        let turn_metrics = self
            .turn_metrics
            .as_ref()
            .filter(|turn| turn.turn_id == turn_id);
        if let Some(turn_metrics) = &turn_metrics {
            turn_metrics.record_plugin(skill.plugin_id.as_deref());
        }
        if let Some(metrics) = &self.metrics {
            let skill_name_tag = sanitize_metric_tag_value(skill.name.as_str());
            let plugin_id_tag =
                sanitize_metric_tag_value(skill.plugin_id.as_deref().unwrap_or("unattributed"));
            let model_slug_tag = sanitize_metric_tag_value(model.as_str());
            let reasoning_effort = turn_metrics
                .as_ref()
                .map(|turn| turn.reasoning_effort.as_str())
                .unwrap_or("unknown");
            let invoke_type = match invocation_type {
                InvocationType::Explicit => "explicit",
                InvocationType::Implicit => "implicit",
            };
            metrics.counter(
                "codex.skill.injected",
                /*inc*/ 1,
                &[
                    ("status", "ok"),
                    ("skill", skill_name_tag.as_str()),
                    ("invoke_type", invoke_type),
                    ("plugin_id", plugin_id_tag.as_str()),
                    ("model_slug", model_slug_tag.as_str()),
                    ("reasoning_effort", reasoning_effort),
                ],
            );
        }
        self.client.track_skill_invocations(
            build_track_events_context(
                model,
                self.thread_id.clone(),
                turn_id,
                self.product_client_id.clone(),
                turn_metrics.and_then(|turn| turn.turn_metadata.clone()),
            ),
            vec![SkillInvocation {
                skill_name: skill.name.clone(),
                location: if let Some(path) = (skill.authority.kind == SkillSourceKind::Host)
                    .then(|| {
                        codex_utils_path_uri::PathUri::from_host_native_path(
                            skill.main_prompt.as_str(),
                        )
                        .ok()
                    })
                    .flatten()
                {
                    SkillInvocationLocation::Host {
                        path,
                        scope: skill
                            .prompt_scope()
                            .unwrap_or(codex_protocol::protocol::SkillScope::User),
                    }
                } else {
                    SkillInvocationLocation::Resource {
                        id: skill.main_prompt.as_str().to_string(),
                        skill_id: skill.canonical_skill_id.clone(),
                        scope: skill.analytics_scope,
                    }
                },
                plugin_id: skill.plugin_id.clone(),
                remote_plugin_id: skill.remote_plugin_id.clone(),
                invocation_type,
            }],
        );
    }
}

#[derive(Clone)]
struct SkillToolContext {
    providers: SkillProviders,
    mcp_resources: Option<Arc<McpResourceClient>>,
    thread_state: Arc<SkillsThreadState>,
    analytics: Option<SkillAnalytics>,
    cloud_available: bool,
    executor_query: Option<SkillListQuery>,
    selected_plugins: Option<Arc<SelectedPluginSnapshot>>,
    executor_catalog: Arc<OnceCell<SkillCatalog>>,
    host_snapshot: Option<Arc<HostSkillsSnapshot>>,
}

impl SkillToolContext {
    async fn catalog(&self, turn_id: &str, authority: SkillToolAuthoritySelector) -> SkillCatalog {
        match authority {
            SkillToolAuthoritySelector::Cloud => {
                if !self.cloud_available {
                    return SkillCatalog::default();
                }
                self.thread_state.cloud_catalog_snapshot()
            }
            SkillToolAuthoritySelector::Executor => {
                let Some(mut query) = self.executor_query.clone() else {
                    return SkillCatalog::default();
                };
                query.turn_id = turn_id.to_string();
                let mut catalog = self
                    .executor_catalog
                    .get_or_init(|| self.providers.list_executor_for_turn(query))
                    .await
                    .clone();
                if let Some(selected_plugins) = &self.selected_plugins {
                    attribute_executor_plugins(&mut catalog, selected_plugins);
                }
                catalog
            }
            SkillToolAuthoritySelector::Host => {
                self.providers
                    .list_for_turn(SkillListQuery {
                        turn_id: turn_id.to_string(),
                        executor_roots: Vec::new(),
                        resolved_executor_roots: Vec::new(),
                        host_snapshot: self.host_snapshot.clone(),
                        include_host_skills: self.host_snapshot.is_some(),
                        include_bundled_skills: self.thread_state.config().bundled_skills_enabled,
                        include_cloud_skills: false,
                        mcp_resources: None,
                        executor_capability_discovery: None,
                    })
                    .await
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SkillToolAuthoritySelector {
    Cloud,
    Executor,
    Host,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SkillToolAuthority {
    Cloud,
    Executor { id: String },
    Host,
}

impl SkillToolAuthority {
    pub(crate) fn from_authority(authority: &SkillAuthority) -> Option<Self> {
        match &authority.kind {
            SkillSourceKind::Cloud if authority.id == CODEX_APPS_MCP_SERVER_NAME => {
                Some(Self::Cloud)
            }
            SkillSourceKind::Executor => Some(Self::Executor {
                id: authority.id.clone(),
            }),
            SkillSourceKind::Host => Some(Self::Host),
            SkillSourceKind::Cloud | SkillSourceKind::Custom(_) => None,
        }
    }
}
