use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::Value;

use crate::plan::types::ROOT_DIRECTORY_INPUT;
use crate::plan::types::{PlanStep, StepConfig};
use crate::tools::catalog::{McpConfig, McpTransport, ToolCatalog, ToolConfig};

pub(super) fn step_uses_workspace(step: &PlanStep, catalog: &ToolCatalog) -> bool {
    match &step.config {
        StepConfig::CodeCall(_) | StepConfig::AgentCall(_) => true,
        StepConfig::ToolCall(config) => catalog.get(&config.tool).is_some_and(|entry| {
            matches!(
                entry.config,
                ToolConfig::Subprocess(_)
                    | ToolConfig::Mcp(McpConfig {
                        transport: McpTransport::Stdio { .. },
                        ..
                    })
            )
        }),
        _ => false,
    }
}

#[derive(Debug)]
pub(crate) struct RunWorkspace {
    pub(crate) root: PathBuf,
    managed: bool,
}

impl RunWorkspace {
    pub(crate) fn resolve(
        config: &IndexMap<String, Value>,
        storage_root: &Path,
        run_id: &str,
    ) -> std::io::Result<Self> {
        let selected = match config.get(&format!("input.{ROOT_DIRECTORY_INPUT}")) {
            Some(Value::String(path)) if !path.is_empty() => Some(PathBuf::from(path)),
            None | Some(Value::Null) | Some(Value::String(_)) => None,
            Some(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "root_directory must be a directory path or null",
                ));
            }
        };
        let managed = selected.is_none();
        let root =
            selected.unwrap_or_else(|| storage_root.join("runs").join(run_id).join("workspace"));
        Ok(Self {
            root: std::path::absolute(root)?,
            managed,
        })
    }

    pub(crate) fn prepare(&self) -> std::io::Result<PathBuf> {
        if self.managed {
            std::fs::create_dir_all(&self.root)?;
        }
        if !self.root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "workspace '{}' is not an existing directory",
                    self.root.display()
                ),
            ));
        }
        self.root.canonicalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_null_and_empty_roots_use_the_same_managed_workspace() {
        let storage = tempfile::tempdir().unwrap();
        let expected = storage.path().join("runs/run-id/workspace");
        for value in [None, Some(Value::Null), Some(Value::String(String::new()))] {
            let config = value
                .map(|value| IndexMap::from([("input.root_directory".to_owned(), value)]))
                .unwrap_or_default();
            let workspace = RunWorkspace::resolve(&config, storage.path(), "run-id").unwrap();
            assert_eq!(workspace.root, expected);
            assert_eq!(
                workspace.prepare().unwrap(),
                expected.canonicalize().unwrap()
            );
        }
    }

    #[test]
    fn selected_root_is_used_without_creating_a_managed_workspace() {
        let storage = tempfile::tempdir().unwrap();
        let selected = tempfile::tempdir().unwrap();
        let config = IndexMap::from([(
            "input.root_directory".to_owned(),
            serde_json::json!(selected.path()),
        )]);
        let workspace = RunWorkspace::resolve(&config, storage.path(), "run-id").unwrap();
        assert_eq!(
            workspace.prepare().unwrap(),
            selected.path().canonicalize().unwrap()
        );
        assert!(!storage.path().join("runs").exists());
    }

    #[test]
    fn invalid_selected_roots_fail_instead_of_being_created_or_replaced() {
        let storage = tempfile::tempdir().unwrap();
        let missing = storage.path().join("missing");
        let file = storage.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();
        for path in [&missing, &file] {
            let config =
                IndexMap::from([("input.root_directory".to_owned(), serde_json::json!(path))]);
            let workspace = RunWorkspace::resolve(&config, storage.path(), "run-id").unwrap();
            assert!(workspace.prepare().is_err());
        }
        assert!(!missing.exists());
        assert!(!storage.path().join("runs").exists());
        let invalid = IndexMap::from([("input.root_directory".to_owned(), serde_json::json!(42))]);
        assert_eq!(
            RunWorkspace::resolve(&invalid, storage.path(), "run-id")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
