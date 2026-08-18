//! Plan export/import bundle format.
//!
//! A [`PlanBundle`] is the on-disk artifact produced by "Export" and consumed
//! by "Import". It carries the full [`Plan`] plus a lightweight *reference*
//! (name, description, schemas) for every tool the plan calls — never the
//! runnable [`ToolConfig`](crate::tools::catalog::ToolConfig), since that
//! commonly holds machine-local paths, server commands, or credentials that
//! would be meaningless (or unsafe) to copy to another machine.
//!
//! On import, a tool whose name already exists in the local catalog is left
//! untouched (the local, presumably correctly-configured, entry wins). A
//! tool that doesn't exist locally is synthesized fresh from its reference —
//! see `Backend::synthesize_tool` in the compiler module.

use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::PlanError;
use crate::plan::types::{Plan, StepConfig};
use crate::plan::{read_file, write_file_atomically};
use crate::tools::catalog::{ToolCatalog, ToolKind};

/// Current on-disk format version. Bump when the shape changes in a way that
/// isn't backward compatible; imports of a newer version are rejected.
pub const CURRENT_FORMAT_VERSION: u32 = 1;

fn empty_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object" })
}

/// A lightweight, non-executable description of a tool referenced by a plan.
///
/// Deliberately excludes `ToolConfig` — subprocess commands, HTTP base URLs,
/// headers, and MCP server invocations are machine-local and may carry
/// secrets, so they are never written to an export bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolReference {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_schema")]
    pub input_schema: serde_json::Value,
    #[serde(default = "empty_schema")]
    pub output_schema: serde_json::Value,
    /// Best-effort hint for which `ToolConfig` shape to synthesize on
    /// import. Never influences validation — purely a prompt nudge.
    #[serde(default)]
    pub kind_hint: Option<ToolKind>,
}

/// The export/import artifact: a plan plus references for every tool it calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanBundle {
    pub format_version: u32,
    pub exported_at: chrono::DateTime<chrono::Utc>,
    pub app_version: String,
    pub plan: Plan,
    #[serde(default)]
    pub tools: Vec<ToolReference>,
}

impl PlanBundle {
    /// Build a bundle from a plan and the catalog it was validated against.
    ///
    /// Returns the bundle plus the names of any tools the plan calls that
    /// are missing from `catalog` — those are still included in the bundle
    /// as bare name-only references, but the caller should warn the user
    /// since there is no description to synthesize a good tool from later.
    pub fn from_plan(plan: &Plan, catalog: &ToolCatalog) -> (Self, Vec<String>) {
        let mut missing = Vec::new();
        let mut seen = IndexSet::new();
        let mut tools = Vec::new();

        for step in &plan.steps {
            let StepConfig::ToolCall(cfg) = &step.config else {
                continue;
            };
            if !seen.insert(cfg.tool.clone()) {
                continue;
            }
            match catalog.get(&cfg.tool) {
                Some(entry) => tools.push(ToolReference {
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    input_schema: entry.input_schema.clone(),
                    output_schema: entry.output_schema.clone(),
                    kind_hint: Some(entry.config.kind()),
                }),
                None => {
                    missing.push(cfg.tool.clone());
                    tools.push(ToolReference {
                        name: cfg.tool.clone(),
                        description: String::new(),
                        input_schema: empty_schema(),
                        output_schema: empty_schema(),
                        kind_hint: None,
                    });
                }
            }
        }

        let bundle = Self {
            format_version: CURRENT_FORMAT_VERSION,
            exported_at: chrono::Utc::now(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            plan: plan.clone(),
            tools,
        };
        (bundle, missing)
    }

    /// Serialise the bundle to a pretty-printed JSON string.
    pub fn to_json(&self) -> Result<String, PlanError> {
        self.preflight()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse a bundle from a JSON string, rejecting unsupported format versions.
    pub fn from_json(json: &str) -> Result<Self, PlanError> {
        let bundle: Self = serde_json::from_str(json)?;
        if bundle.format_version > CURRENT_FORMAT_VERSION {
            return Err(PlanError::Invalid(format!(
                "bundle format version {} is newer than this app supports ({}) — update the app",
                bundle.format_version, CURRENT_FORMAT_VERSION
            )));
        }
        bundle.preflight()?;
        Ok(bundle)
    }

    /// Verify that tool references are a one-to-one set projection of the
    /// distinct TOOL_CALL names in the bundled plan.
    pub fn preflight(&self) -> Result<(), PlanError> {
        let expected: IndexSet<&str> = self
            .plan
            .steps
            .iter()
            .filter_map(|step| match &step.config {
                StepConfig::ToolCall(config) => Some(config.tool.as_str()),
                _ => None,
            })
            .collect();
        let mut actual = IndexSet::new();
        let mut duplicates = IndexSet::new();
        for reference in &self.tools {
            if !actual.insert(reference.name.as_str()) {
                duplicates.insert(reference.name.as_str());
            }
        }

        let missing: Vec<&str> = expected
            .iter()
            .copied()
            .filter(|name| !actual.contains(name))
            .collect();
        let extraneous: Vec<&str> = actual
            .iter()
            .copied()
            .filter(|name| !expected.contains(name))
            .collect();
        if duplicates.is_empty() && missing.is_empty() && extraneous.is_empty() {
            return Ok(());
        }

        let mut problems = Vec::new();
        if !duplicates.is_empty() {
            problems.push(format!(
                "duplicate tool references: {}",
                duplicates.iter().copied().collect::<Vec<_>>().join(", ")
            ));
        }
        if !missing.is_empty() {
            problems.push(format!("missing tool references: {}", missing.join(", ")));
        }
        if !extraneous.is_empty() {
            problems.push(format!(
                "extraneous tool references: {}",
                extraneous.join(", ")
            ));
        }
        Err(PlanError::Invalid(format!(
            "bundle tool references do not match plan TOOL_CALL steps ({})",
            problems.join("; ")
        )))
    }

    /// Persist the bundle to a file (creating parent directories).
    pub fn save_to_file(&self, path: &Path) -> Result<(), PlanError> {
        write_file_atomically(path, &self.to_json()?)
    }

    /// Load a bundle from a file.
    pub fn load_from_file(path: &Path) -> Result<Self, PlanError> {
        let raw = read_file(path)?;
        Self::from_json(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{AgentCallConfig, PlanMetadata, PlanStep, ToolCallConfig};
    use crate::tools::catalog::{SubprocessConfig, ToolConfig, ToolEntry};
    use indexmap::IndexMap;

    fn sample_plan(tool_names: &[&str]) -> Plan {
        let steps = tool_names
            .iter()
            .enumerate()
            .map(|(i, tool)| PlanStep {
                id: format!("step_{i}"),
                name: format!("Step {i}"),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: (*tool).to_owned(),
                    arguments: IndexMap::new(),
                }),
                depends_on: Vec::new(),
                outputs: Vec::new(),
                timeout_secs: None,
                retry: None,
            })
            .collect();

        Plan {
            metadata: PlanMetadata::new(Some("test intent".to_owned())),
            name: "Test plan".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps,
            outputs: Vec::new(),
        }
    }

    fn sample_catalog() -> ToolCatalog {
        ToolCatalog::new(vec![ToolEntry {
            name: "echo".to_owned(),
            description: "Echoes its input".to_owned(),
            config: ToolConfig::Subprocess(SubprocessConfig {
                command: "echo".to_owned(),
                args: vec![],
                env: IndexMap::new(),
                working_dir: None,
            }),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            allowlisted: true,
            timeout_secs: None,
        }])
    }

    #[test]
    fn from_plan_collects_distinct_tool_references() {
        let plan = sample_plan(&["echo", "echo", "curl"]);
        let catalog = sample_catalog();
        let (bundle, missing) = PlanBundle::from_plan(&plan, &catalog);

        assert_eq!(bundle.tools.len(), 2);
        assert_eq!(bundle.tools[0].name, "echo");
        assert_eq!(bundle.tools[0].description, "Echoes its input");
        assert_eq!(bundle.tools[1].name, "curl");
        assert_eq!(missing, vec!["curl".to_owned()]);
    }

    #[test]
    fn from_plan_never_includes_tool_config() {
        let plan = sample_plan(&["echo"]);
        let catalog = sample_catalog();
        let (bundle, _) = PlanBundle::from_plan(&plan, &catalog);
        let json = bundle.to_json().unwrap();
        assert!(!json.contains("\"command\""));
        assert!(!json.contains("\"kind\": \"subprocess\""));
    }

    #[test]
    fn round_trips_through_json() {
        let plan = sample_plan(&["echo"]);
        let catalog = sample_catalog();
        let (bundle, _) = PlanBundle::from_plan(&plan, &catalog);

        let json = bundle.to_json().unwrap();
        let parsed = PlanBundle::from_json(&json).unwrap();
        assert_eq!(parsed, bundle);
    }

    #[test]
    fn round_trips_agent_call_without_portable_tool_references() {
        let mut plan = sample_plan(&[]);
        plan.steps.push(PlanStep {
            id: "implement".to_owned(),
            name: "Implement change".to_owned(),
            description: None,
            config: StepConfig::AgentCall(AgentCallConfig {
                objective: "Implement the approved change and run its tests".to_owned(),
                working_dir: "${input.root_directory}".to_owned(),
                timeout_secs: Some(900),
            }),
            depends_on: Vec::new(),
            outputs: Vec::new(),
            timeout_secs: None,
            retry: None,
        });

        let (bundle, missing) = PlanBundle::from_plan(&plan, &sample_catalog());
        assert!(bundle.tools.is_empty());
        assert!(missing.is_empty());

        let parsed = PlanBundle::from_json(&bundle.to_json().unwrap()).unwrap();
        assert_eq!(parsed.plan, plan);
    }

    /// A plan whose `root_directory` input is
    /// optional with no default (app-managed scratch workspace) must
    /// round-trip through the export/import bundle unchanged — still
    /// optional, still default-free.
    #[test]
    fn round_trips_optional_root_directory_input_without_default() {
        use crate::plan::types::{PlanInput, ROOT_DIRECTORY_INPUT};

        let mut plan = sample_plan(&["echo"]);
        plan.inputs = vec![PlanInput {
            name: ROOT_DIRECTORY_INPUT.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: false,
            default: None,
            input_kind: crate::plan::types::InputKind::Value,
        }];
        let catalog = sample_catalog();
        let (bundle, _) = PlanBundle::from_plan(&plan, &catalog);

        let json = bundle.to_json().unwrap();
        let parsed = PlanBundle::from_json(&json).unwrap();

        assert_eq!(parsed.plan.inputs, plan.inputs);
        assert_eq!(parsed.plan.inputs.len(), 1);
        assert!(!parsed.plan.inputs[0].required);
        assert!(parsed.plan.inputs[0].default.is_none());
    }

    #[test]
    fn rejects_newer_format_version() {
        let plan = sample_plan(&["echo"]);
        let catalog = sample_catalog();
        let (mut bundle, _) = PlanBundle::from_plan(&plan, &catalog);
        bundle.format_version = CURRENT_FORMAT_VERSION + 1;

        let json = bundle.to_json().unwrap();
        assert!(PlanBundle::from_json(&json).is_err());
    }

    #[test]
    fn rejects_missing_tool_reference() {
        let plan = sample_plan(&["echo", "curl"]);
        let catalog = sample_catalog();
        let (mut bundle, _) = PlanBundle::from_plan(&plan, &catalog);
        bundle.tools.retain(|reference| reference.name != "curl");

        let json = serde_json::to_string(&bundle).unwrap();
        let error = PlanBundle::from_json(&json).unwrap_err();

        assert!(error.to_string().contains("missing tool references: curl"));
    }

    #[test]
    fn rejects_duplicate_tool_reference() {
        let plan = sample_plan(&["echo"]);
        let catalog = sample_catalog();
        let (mut bundle, _) = PlanBundle::from_plan(&plan, &catalog);
        bundle.tools.push(bundle.tools[0].clone());

        let json = serde_json::to_string(&bundle).unwrap();
        let error = PlanBundle::from_json(&json).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("duplicate tool references: echo")
        );
    }

    #[test]
    fn rejects_extraneous_tool_reference() {
        let plan = sample_plan(&["echo"]);
        let catalog = sample_catalog();
        let (mut bundle, _) = PlanBundle::from_plan(&plan, &catalog);
        let mut extra = bundle.tools[0].clone();
        extra.name = "unused".to_owned();
        bundle.tools.push(extra);

        let json = serde_json::to_string(&bundle).unwrap();
        let error = PlanBundle::from_json(&json).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("extraneous tool references: unused")
        );
    }

    #[test]
    fn bundle_file_save_atomically_replaces_existing_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("bundle.json");
        let catalog = sample_catalog();
        let (first, _) = PlanBundle::from_plan(&sample_plan(&["echo"]), &catalog);
        first.save_to_file(&path).unwrap();

        let (second, _) = PlanBundle::from_plan(&sample_plan(&["echo", "curl"]), &catalog);
        second.save_to_file(&path).unwrap();

        let loaded = PlanBundle::load_from_file(&path).unwrap();
        assert_eq!(loaded.plan.steps.len(), 2);
        assert_eq!(loaded.tools.len(), 2);
        let siblings = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(siblings.len(), 1, "temporary files must be cleaned up");
    }
}
