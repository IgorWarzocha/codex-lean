use std::io;
use std::sync::Arc;

use crate::SkillLoadOutcome;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::LOCAL_FS;
use codex_skills::SkillMetadata;
use codex_utils_path_uri::PathUri;

use crate::package::PackageAccess;

/// Immutable snapshot of host-owned skills and their source filesystems.
#[derive(Debug, Clone)]
pub struct HostSkillsSnapshot {
    outcome: Arc<SkillLoadOutcome>,
}

impl HostSkillsSnapshot {
    pub fn new(outcome: Arc<SkillLoadOutcome>) -> Self {
        Self { outcome }
    }

    pub fn outcome(&self) -> &SkillLoadOutcome {
        self.outcome.as_ref()
    }

    pub async fn read_skill_text(&self, skill: &SkillMetadata) -> io::Result<String> {
        self.outcome.read_skill_text(skill).await
    }

    pub(crate) fn file_system_for_skill(
        &self,
        skill: &SkillMetadata,
    ) -> Arc<dyn ExecutorFileSystem> {
        self.outcome
            .file_system_for_skill(skill)
            .unwrap_or_else(|| Arc::clone(&LOCAL_FS))
    }

    pub(crate) async fn package_files(
        &self,
        skill: &SkillMetadata,
    ) -> io::Result<Vec<(String, PathUri)>> {
        let main = &skill.path_to_skills_md;
        let root = main
            .parent()
            .ok_or_else(|| io::Error::other("skill has no package directory"))?;
        let fs = self.file_system_for_skill(skill);
        PackageAccess::Host(fs.as_ref()).files(&root).await
    }
}
