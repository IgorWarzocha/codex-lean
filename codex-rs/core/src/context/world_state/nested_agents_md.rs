use super::PreviousSectionState;
use super::SectionTransition;
use super::WorldStateSection;
use super::WorldStateUpdate;
use crate::agents_md::nested::NestedAgentsMd;
use crate::agents_md::nested::NestedInstruction;
use crate::context::UserInstructions;

pub(crate) struct NestedAgentsMdState(pub(crate) NestedAgentsMd);

impl WorldStateSection for NestedAgentsMdState {
    const ID: &'static str = "nested_agents_md";
    type Snapshot = NestedAgentsMd;

    // Root instructions own legacy unscoped fragments. Nested visibility is persisted separately.
    fn matches_legacy_fragment(_role: &str, _text: &str) -> bool {
        false
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> SectionTransition<Self::Snapshot> {
        let previous = match previous {
            PreviousSectionState::Known(previous) => Some(previous),
            PreviousSectionState::Absent | PreviousSectionState::Unknown => None,
        };
        if previous.is_none() && self.0 == NestedAgentsMd::default() {
            return (None, Vec::new());
        }
        if previous == Some(&self.0) {
            return (None, Vec::new());
        }
        let mut blocks = Vec::new();
        if let Some(previous) = previous {
            for entry in &previous.entries {
                if !self
                    .0
                    .entries
                    .iter()
                    .any(|current| same_source(current, entry))
                {
                    blocks.push(format!(
                        "Instructions from {} in environment `{}` no longer apply.",
                        entry.source_path.inferred_native_path_string(),
                        entry.environment_id
                    ));
                }
            }
        }
        for entry in &self.0.entries {
            if previous.is_some_and(|previous| previous.entries.contains(entry)) {
                continue;
            }
            let updated = previous
                .is_some_and(|previous| previous.entries.iter().any(|old| same_source(old, entry)));
            let directory = entry
                .source_path
                .parent()
                .unwrap_or_else(|| entry.source_path.clone());
            blocks.push(format!(
                "{}Instructions from {} in environment `{}` apply only to paths beneath {}.\n\n{}",
                if updated {
                    "Replaces the previous instructions from this file.\n"
                } else {
                    ""
                },
                entry.source_path.inferred_native_path_string(),
                entry.environment_id,
                directory.inferred_native_path_string(),
                entry.contents,
            ));
        }
        let fragment = (!blocks.is_empty()).then(|| {
            Box::new(UserInstructions {
                directory: None,
                text: blocks.join("\n\n"),
            }) as Box<dyn crate::context::ContextualUserFragment>
        });
        (
            Some(self.0.clone()),
            WorldStateUpdate::optional_boxed_fragment(fragment),
        )
    }
}

fn same_source(left: &NestedInstruction, right: &NestedInstruction) -> bool {
    left.environment_id == right.environment_id && left.source_path == right.source_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::world_state::test_support::FragmentSectionTestExt;
    use codex_utils_path_uri::PathUri;

    #[test]
    fn nested_visibility_deduplicates_updates_and_reconstructs_without_root_replacement() {
        let mut current = NestedAgentsMdState(NestedAgentsMd {
            targets: Vec::new(),
            entries: vec![NestedInstruction {
                environment_id: "local".to_string(),
                source_path: PathUri::parse("file:///repo/src/AGENTS.md").unwrap(),
                contents: "use formatter".to_string(),
            }],
        });
        let (snapshot, fragment) = current.render_fragment_diff(PreviousSectionState::Absent);
        let initial = fragment.unwrap();
        assert_eq!(initial.role(), "user");
        assert!(initial.render().contains("only to paths beneath /repo/src"));
        assert!(
            !initial
                .render()
                .contains("Prior AGENTS.md instructions replaced")
        );
        // Persisted snapshots use the same comparison after resume.
        let restored = serde_json::from_value::<NestedAgentsMd>(
            serde_json::to_value(snapshot.unwrap()).unwrap(),
        )
        .unwrap();
        assert!(
            current
                .render_fragment_diff(PreviousSectionState::Known(&restored))
                .1
                .is_none()
        );
        current.0.entries[0].contents = "new formatter".to_string();
        let update = current
            .render_fragment_diff(PreviousSectionState::Known(&restored))
            .1
            .unwrap()
            .render();
        assert!(update.contains("Replaces the previous instructions from this file"));
        assert!(!update.contains("use formatter"));
        // A new window has no visible baseline and must receive current guidance again.
        assert!(
            current
                .render_fragment_diff(PreviousSectionState::Absent)
                .1
                .unwrap()
                .render()
                .contains("new formatter")
        );
        current.0.entries.clear();
        assert!(
            current
                .render_fragment_diff(PreviousSectionState::Known(&restored))
                .1
                .unwrap()
                .render()
                .contains("no longer apply")
        );
    }
}
