//! Resolves model-owned messages from catalog values and bundled defaults.
//! Resolution preserves sparse catalog data and source identity, with family-specific validation.
//! Prompt composition and runtime settings remain with consumers; each accessor
//! selects and resolves only the requested message family.

use codex_protocol::openai_models::CodeModeToolMessages;
use codex_protocol::openai_models::ConfirmationPolicies;
use codex_protocol::openai_models::IndirectDescriptionPrefixes;
use codex_protocol::openai_models::McpResourceToolMessages;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ToolMessage;
use permissions::ResolvedApprovalMessages;
use permissions::ResolvedPermissionMessages;

mod collaboration;
mod guardian;
mod multi_agent;
pub(crate) mod permissions;
mod workflow;

pub use collaboration::ResolvedCollaborationModeMessages;
pub use guardian::ResolvedAutoReviewMessages;
pub use multi_agent::ResolvedMultiAgentMessages;
pub use workflow::apply_default_catalog_workflow;

/// Text together with whether it was supplied by the catalog, even when it equals the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedMessage<'a> {
    Catalog(&'a str),
    Bundled(&'static str),
}

impl<'a> ResolvedMessage<'a> {
    pub(crate) fn new(catalog: Option<&'a str>, bundled: &'static str) -> Self {
        catalog.map_or(Self::Bundled(bundled), Self::Catalog)
    }

    pub fn text(self) -> &'a str {
        match self {
            Self::Catalog(text) | Self::Bundled(text) => text,
        }
    }

    pub fn catalog_override(self) -> Option<&'a str> {
        match self {
            Self::Catalog(text) => Some(text),
            Self::Bundled(_) => None,
        }
    }
}

const REMINDER_MESSAGE_TEMPLATE: &str = concat!(
    "Your context window is nearly exhausted (only {n_remaining} tokens remaining) and will be automatically reset for you soon. ",
    "Once reset, message items in current context window will be cleared in the new window, but notes and history items will be persistent across windows."
);
const PERSISTENT_INSTRUCTIONS: &str = include_str!("../templates/persistent_mode.md");
const CONTENT_FILTER_GUIDANCE: &str = "Your previous response was blocked by a content filter. Do not treat this as a transient failure or try to reproduce or work around the blocked content through repeated attempts, altered formatting, splitting, encoding, tools, subagents, or later wakes. Briefly explain the limitation and offer a permitted alternative. Continue unrelated authorized work.";
const MAX_CONTENT_FILTER_GUIDANCE_BYTES: usize = 512;

/// Resolves model-owned text from catalog overrides or bundled defaults, one family at a time.
///
/// The view borrows the consumer's captured model metadata without resolving any families.
/// Missing values select bundled text or retain delegation to consumer-owned settings.
/// Explicit empty strings remain overrides unless a message family validates them.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedModelMessages<'a> {
    catalog_messages: Option<&'a ModelMessages>,
}

impl<'a> ResolvedModelMessages<'a> {
    /// Borrows messages from the selected model; absent catalog values still use defaults.
    pub fn from_model(model_info: &'a ModelInfo) -> Self {
        Self {
            catalog_messages: model_info.model_messages.as_ref(),
        }
    }

    /// Resolves only bundled defaults, independently of any selected model.
    pub fn bundled() -> Self {
        Self {
            catalog_messages: None,
        }
    }

    /// Resolves the literal instruction template, preserving missing values.
    pub fn instructions_template(&self) -> Option<&'a str> {
        self.catalog_messages
            .and_then(|messages| messages.instructions_template.as_deref())
    }

    /// Resolves approval messages and their bundled alternatives.
    pub(crate) fn approvals(self) -> ResolvedApprovalMessages<'a> {
        ResolvedApprovalMessages::new(
            self.catalog_messages
                .and_then(|messages| messages.approvals.as_ref()),
        )
    }

    /// Resolves permission templates without preparing runtime permission facts.
    pub(crate) fn permissions(self) -> ResolvedPermissionMessages<'a> {
        ResolvedPermissionMessages::new(
            self.catalog_messages
                .and_then(|messages| messages.permissions.as_ref()),
        )
    }

    /// Resolves mode overrides while retaining bundled alternatives and absent values.
    pub fn collaboration_modes(&self) -> ResolvedCollaborationModeMessages<'a> {
        ResolvedCollaborationModeMessages::new(
            self.catalog_messages
                .and_then(|messages| messages.collaboration_modes.as_ref()),
        )
    }

    /// Resolves multi-agent messages while preserving their catalog or bundled source.
    pub fn multi_agent(&self) -> ResolvedMultiAgentMessages<'a> {
        ResolvedMultiAgentMessages::new(
            self.catalog_messages
                .and_then(|messages| messages.multi_agent.as_ref()),
        )
    }

    /// Resolves auto-review policy messages and rejection/timeout instructions.
    pub fn auto_review(&self) -> ResolvedAutoReviewMessages<'a> {
        ResolvedAutoReviewMessages::new(
            self.catalog_messages
                .and_then(|messages| messages.auto_review.as_ref()),
        )
    }

    /// Resolves classifier instructions from catalog text or the bundled default.
    pub fn guardian_classifier_instructions(&self) -> &'a str {
        guardian::classifier_instructions(self.catalog_messages)
    }

    /// Resolves reminder text without selecting or validating runtime budget settings.
    pub fn token_budget_reminder_template(&self) -> &'a str {
        self.catalog_messages
            .and_then(|messages| messages.token_budget.as_ref())
            .map_or(REMINDER_MESSAGE_TEMPLATE, |messages| {
                messages.reminder_message_template.as_str()
            })
    }

    /// Resolves optional confirmation policies, leaving missing values to actor defaults.
    pub fn confirmation_policies(&self) -> Option<&'a ConfirmationPolicies> {
        self.catalog_messages
            .and_then(|messages| messages.confirmation_policies.as_ref())
    }

    /// Selects a V2 tool's static description by its name, independently of its runtime namespace.
    /// Missing text retains the tool's bundled description; an empty string replaces it.
    pub fn multi_agent_tool_description_override(&self, tool_name: &str) -> Option<&'a str> {
        self.multi_agent_tool(tool_name)?.description.as_deref()
    }

    /// Selects a V2 tool's complete parameter schema; parsing belongs to the tool consumer.
    pub fn multi_agent_tool_parameters_override(&self, tool_name: &str) -> Option<&'a str> {
        self.multi_agent_tool(tool_name)?.parameters.as_deref()
    }

    fn multi_agent_tool(self, tool_name: &str) -> Option<&'a ToolMessage> {
        self.catalog_messages
            .and_then(|messages| messages.tools.as_ref())
            .and_then(|tools| tools.multi_agent.as_ref())?
            .by_name(tool_name)
    }

    /// Selects resource helper messages; schema parsing belongs to the tool owner.
    pub fn mcp_resources(&self) -> Option<&'a McpResourceToolMessages> {
        self.catalog_messages?
            .tools
            .as_ref()?
            .mcp_resources
            .as_ref()
    }

    /// Selects indirect tool guidance; tool rendering owns namespace mapping and normalization.
    pub fn indirect_description_prefixes(&self) -> Option<&'a IndirectDescriptionPrefixes> {
        self.catalog_messages?
            .tools
            .as_ref()?
            .indirect_description_prefixes
            .as_ref()
    }

    /// Selects Code Mode messages; bundled text and runtime composition belong to the tool owner.
    pub fn code_mode(&self) -> Option<&'a CodeModeToolMessages> {
        self.catalog_messages?.tools.as_ref()?.code_mode.as_ref()
    }

    /// Selects wait's complete description.
    pub fn code_mode_wait_description_override(&self) -> Option<&'a str> {
        self.code_mode()?.wait.as_ref()?.description.as_deref()
    }

    /// Selects wait's parameter schema. Exec uses a harness-owned freeform grammar.
    pub fn code_mode_wait_parameters_override(&self) -> Option<&'a str> {
        self.code_mode()?.wait.as_ref()?.parameters.as_deref()
    }

    /// Resolves persistent-mode instructions without deciding whether the mode is active.
    pub fn persistent_instructions(&self) -> &'a str {
        self.catalog_messages
            .and_then(|messages| messages.persistent_instructions.as_deref())
            .unwrap_or(PERSISTENT_INSTRUCTIONS)
    }

    /// Resolves bounded recovery guidance, falling back without truncating instructions.
    pub fn content_filter_guidance(&self) -> &'a str {
        self.catalog_messages
            .and_then(|messages| messages.content_filter_guidance.as_deref())
            .filter(|text| {
                !text.trim().is_empty() && text.len() <= MAX_CONTENT_FILTER_GUIDANCE_BYTES
            })
            .unwrap_or(CONTENT_FILTER_GUIDANCE)
    }
}
