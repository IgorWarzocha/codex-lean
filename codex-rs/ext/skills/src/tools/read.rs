use codex_analytics::InvocationType;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;

use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::package::PackageAccess;
use crate::provider::SkillReadContext;
use crate::provider::SkillReadRequest;

use super::SkillToolContext;
use super::selection::PackageFile;
use super::selection::SkillPackage;
use super::selection::select;

fn error(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

impl SkillToolContext {
    fn host_skill(
        &self,
        entry: &SkillCatalogEntry,
    ) -> Result<&codex_skills::SkillMetadata, FunctionCallError> {
        self.host_snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot.outcome().skills.iter().find(|skill| {
                    snapshot.outcome().is_skill_enabled(skill)
                        && skill
                            .path_to_skills_md
                            .inferred_native_path_string()
                            .replace('\\', "/")
                            == entry.main_prompt.as_str().replace('\\', "/")
                })
            })
            .ok_or_else(|| error("Host skill is not available in this snapshot"))
    }

    async fn package_files(
        &self,
        entry: &SkillCatalogEntry,
        call: &ToolCall<'_>,
    ) -> Result<Vec<PackageFile>, FunctionCallError> {
        let files = match entry.authority.kind {
            SkillSourceKind::Host => {
                let snapshot = self
                    .host_snapshot
                    .as_ref()
                    .ok_or_else(|| error("Host skills snapshot is not available"))?;
                snapshot
                    .package_files(self.host_skill(entry)?)
                    .await
                    .map_err(|err| {
                        error(format!(
                            "Failed to list skill package {}: {err}",
                            entry.name
                        ))
                    })?
            }
            SkillSourceKind::Executor => {
                let (id, main) = entry
                    .main_prompt
                    .environment_path()
                    .ok_or_else(|| error("Skill resource is not bound to an environment"))?;
                let root = main
                    .parent()
                    .ok_or_else(|| error("Skill has no package directory"))?;
                let fs = call
                    .environments
                    .iter()
                    .find(|environment| environment.environment_id == id)
                    .map(codex_extension_api::ToolEnvironment::fs)
                    .ok_or_else(|| error("Skill environment is not available for this callback"))?;
                PackageAccess::Executor(fs)
                    .files(&root)
                    .await
                    .map_err(|err| {
                        error(format!(
                            "Failed to list skill package {}: {err}",
                            entry.name
                        ))
                    })?
            }
            SkillSourceKind::Cloud | SkillSourceKind::Custom(_) => return Ok(Vec::new()),
        };
        files
            .into_iter()
            .map(|(relative, path)| {
                let resource = if entry.authority.kind == SkillSourceKind::Host {
                    SkillResourceId::new(path.inferred_native_path_string())
                } else {
                    entry
                        .main_prompt
                        .bind_environment_package_resource(
                            &entry.id,
                            format!("{}/{}", entry.id.0, relative),
                        )
                        .ok_or_else(|| {
                            error("Skill inventory resource does not match its package")
                        })?
                };
                Ok(PackageFile {
                    relative,
                    source: if entry.authority.kind == SkillSourceKind::Executor {
                        resource.as_str().to_string()
                    } else {
                        path.inferred_native_path_string()
                    },
                    resource,
                })
            })
            .collect()
    }

    async fn read_resource(
        &self,
        entry: &SkillCatalogEntry,
        resource: SkillResourceId,
        call: &ToolCall<'_>,
    ) -> Result<String, FunctionCallError> {
        let context = match entry.authority.kind {
            SkillSourceKind::Host => SkillReadContext::Host {
                host_snapshot: self.host_snapshot.clone(),
            },
            SkillSourceKind::Executor => {
                let (id, _) = resource
                    .environment_path()
                    .ok_or_else(|| error("Skill resource is not bound to an environment"))?;
                let fs = call
                    .environments
                    .iter()
                    .find(|environment| environment.environment_id == id)
                    .map(codex_extension_api::ToolEnvironment::fs)
                    .ok_or_else(|| error("Skill environment is not available for this callback"))?;
                SkillReadContext::Executor { fs }
            }
            SkillSourceKind::Cloud => SkillReadContext::Cloud {
                mcp_resources: self.mcp_resources.clone(),
            },
            SkillSourceKind::Custom(_) => SkillReadContext::Custom {
                mcp_resources: self.mcp_resources.clone(),
            },
        };
        let result = self
            .thread_state
            .read_skill(
                &self.providers,
                SkillReadRequest {
                    authority: entry.authority.clone(),
                    package: entry.id.clone(),
                    resource: resource.clone(),
                    context,
                },
            )
            .await
            .map_err(|err| {
                error(format!(
                    "Failed to read skill resource {}: {}",
                    resource.as_str(),
                    err.message
                ))
            })?;
        if result.resource != resource {
            return Err(FunctionCallError::Fatal(
                "Skill provider returned a different resource".to_string(),
            ));
        }
        Ok(result.contents)
    }

    pub(super) async fn read(
        &self,
        entries: Vec<SkillCatalogEntry>,
        names: &[&str],
        call: &ToolCall<'_>,
    ) -> Result<(String, bool), FunctionCallError> {
        let needs_references = names.iter().enumerate().any(|(index, name)| {
            !entries.iter().any(|entry| {
                entry.name == *name
                    || entry.id.relative_resource_path(name).is_some()
                    || (index == 0 && (entry.main_prompt.as_str() == *name || entry.id.0 == *name))
            })
        });
        let mut packages = Vec::with_capacity(entries.len());
        for entry in entries {
            let files = if needs_references {
                self.package_files(&entry, call).await?
            } else {
                Vec::new()
            };
            packages.push(SkillPackage { entry, files });
        }
        let selections = select(&packages, names)?;
        let only_references = selections
            .iter()
            .all(|selection| selection.reference.is_some());
        let one_skill = selections
            .iter()
            .all(|selection| selection.skill == selections[0].skill);
        let mut sections = Vec::new();
        let mut sources = Vec::new();
        let mut external = false;
        let mut output_bytes = 0usize;
        for selection in &selections {
            let package = &mut packages[selection.skill];
            let contents = self
                .read_resource(&package.entry, selection.resource.clone(), call)
                .await?;
            external |= package.entry.authority.kind == SkillSourceKind::Cloud;
            let body = if selection.reference.is_some() {
                contents.trim()
            } else {
                skill_body(&contents)
            };
            if let Some(reference) = &selection.reference {
                sources.push(source_for_resource(package, &selection.resource));
                let label = if only_references && one_skill {
                    reference.clone()
                } else {
                    format!("{}/references/{reference}", package.entry.name)
                };
                sections.push(if selections.len() == 1 {
                    body.to_string()
                } else {
                    format!("--- {label} ---\n{body}")
                });
            } else {
                if package.files.is_empty() {
                    package.files = self.package_files(&package.entry, call).await?;
                }
                let inventory = if package.entry.authority.kind == SkillSourceKind::Cloud
                    || matches!(package.entry.authority.kind, SkillSourceKind::Custom(_))
                {
                    format!(
                        "---\nPackage: {}\nSource: {}",
                        package.entry.id.0,
                        package.entry.main_prompt.as_str()
                    )
                } else {
                    format!(
                        "---\nSkill paths ({}):\n{}",
                        package.files.len(),
                        package
                            .files
                            .iter()
                            .map(|file| format!("- {}", file.source))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                let root = package
                    .entry
                    .main_prompt
                    .environment_path()
                    .and_then(|(id, main)| {
                        main.parent().map(|root| {
                            format!(
                                "\n\nPackage: {}\nSkill root ({id}): {}",
                                package.entry.id.0,
                                root.inferred_native_path_string()
                            )
                        })
                    })
                    .unwrap_or_default();
                let skill = format!("{body}{root}\n\n{inventory}");
                sections.push(if selections.len() == 1 {
                    skill
                } else {
                    format!("--- {} ---\n{skill}", package.entry.name)
                });
                if let Some(analytics) = &self.analytics {
                    analytics.track_skill_invocation(
                        &package.entry,
                        call.model.clone(),
                        call.turn_id.clone(),
                        InvocationType::Implicit,
                    );
                }
                if let Some(state) = self.thread_state.shadow_selection_turn(&call.turn_id) {
                    state.record_invocation(package.entry.main_prompt.as_str());
                }
            }
            output_bytes += sections.last().map_or(0, String::len);
            if output_bytes > super::command::MAX_OUTPUT_BYTES {
                return Err(error(
                    "Skills output exceeds 48 KiB. Read fewer skills or selected references",
                ));
            }
        }
        let mut output = sections.join("\n\n");
        if !sources.is_empty() {
            output.push_str(&format!(
                "\n\n---\nSources:\n{}",
                sources
                    .iter()
                    .map(|source| format!("- {source}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        Ok((output, external))
    }
}

fn source_for_resource(package: &SkillPackage, resource: &SkillResourceId) -> String {
    package
        .files
        .iter()
        .find(|file| &file.resource == resource)
        .map(|file| file.source.clone())
        .unwrap_or_else(|| resource.as_str().to_string())
}

fn skill_body(contents: &str) -> &str {
    let contents = contents.trim_start_matches('\u{feff}');
    let Some(rest) = contents
        .strip_prefix("---")
        .filter(|rest| rest.starts_with(['\n', '\r']))
    else {
        return contents.trim();
    };
    let mut offset = contents.len() - rest.len();
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if matches!(line.trim(), "---" | "...") {
            return contents[offset..].trim();
        }
    }
    contents.trim()
}
