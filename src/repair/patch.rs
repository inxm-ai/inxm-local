//! Low-level patch application helpers.
//!
//! These functions are pure: they receive an owned `Plan` and return a
//! modified `Plan` (or an error string).  They do NOT normalise, validate,
//! or persist — that is the responsibility of `repair::apply_patch`.

use crate::plan::types::{Plan, PlanStep};
use crate::storage::patches::{Patch, PatchOperation};
use thiserror::Error;

// ─── apply_operation ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PatchApplicationError {
    #[error("batch patch must contain at least one operation")]
    EmptyBatch,

    #[error("step {step_id} not found")]
    StepNotFound { step_id: String },

    #[error("{context}: {source}")]
    Serialization {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("{message}")]
    InvalidJsonPointer { message: String },

    #[error("SetPlanField cannot replace the JSON root")]
    PlanRootReplacement,

    #[error("plan metadata is lifecycle-owned and cannot be patched at '{pointer}'")]
    ProtectedPlanMetadata { pointer: String },

    #[error("step '{step_id}' is invalid after patch: {source}")]
    InvalidStep {
        step_id: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("plan is invalid after patch: {0}")]
    InvalidPlan(#[source] serde_json::Error),
}

/// Apply the patch operation to the plan's step list.
///
/// Returns the modified plan or a typed application error. The caller is
/// responsible for normalisation and validation.
pub fn apply_operation(mut plan: Plan, patch: &Patch) -> Result<Plan, PatchApplicationError> {
    apply_single_operation(&mut plan, &patch.operation, &patch.failing_step_id)?;
    Ok(plan)
}

fn apply_single_operation(
    plan: &mut Plan,
    operation: &PatchOperation,
    failing_step_id: &str,
) -> Result<(), PatchApplicationError> {
    match operation {
        PatchOperation::Batch { operations } => {
            if operations.is_empty() {
                return Err(PatchApplicationError::EmptyBatch);
            }
            for op in operations {
                apply_single_operation(plan, op, failing_step_id)?;
            }
        }

        PatchOperation::ReplaceStep { new_step } => {
            let pos = step_position(plan, failing_step_id)?;
            plan.steps[pos] = new_step.clone();
        }

        PatchOperation::UpdateStepConfig { new_config } => {
            let step = step_mut(plan, failing_step_id)?;
            step.config = new_config.clone();
        }

        PatchOperation::InsertBefore { step } => {
            let pos = step_position(plan, failing_step_id)?;
            plan.steps.insert(pos, step.clone());
        }

        PatchOperation::InsertAfter { step } => {
            let pos = step_position(plan, failing_step_id)?;
            plan.steps.insert(pos + 1, step.clone());
        }

        PatchOperation::SetStepField {
            step_id,
            pointer,
            value,
        } => {
            let step = step_mut(plan, step_id)?;
            let mut json = serde_json::to_value(&*step).map_err(|source| {
                PatchApplicationError::Serialization {
                    context: format!("failed to serialise step '{step_id}'"),
                    source,
                }
            })?;
            set_json_pointer(&mut json, pointer, value.clone())?;
            *step = serde_json::from_value::<PlanStep>(json).map_err(|source| {
                PatchApplicationError::InvalidStep {
                    step_id: step_id.clone(),
                    source,
                }
            })?;
        }

        PatchOperation::RemoveStepField { step_id, pointer } => {
            let step = step_mut(plan, step_id)?;
            let mut json = serde_json::to_value(&*step).map_err(|source| {
                PatchApplicationError::Serialization {
                    context: format!("failed to serialise step '{step_id}'"),
                    source,
                }
            })?;
            remove_json_pointer(&mut json, pointer)?;
            *step = serde_json::from_value::<PlanStep>(json).map_err(|source| {
                PatchApplicationError::InvalidStep {
                    step_id: step_id.clone(),
                    source,
                }
            })?;
        }

        PatchOperation::SetPlanField { pointer, value } => {
            validate_plan_pointer(pointer, true)?;
            let mut json = serde_json::to_value(&*plan).map_err(|source| {
                PatchApplicationError::Serialization {
                    context: "failed to serialise plan".to_owned(),
                    source,
                }
            })?;
            set_json_pointer(&mut json, pointer, value.clone())?;
            *plan =
                serde_json::from_value::<Plan>(json).map_err(PatchApplicationError::InvalidPlan)?;
        }

        PatchOperation::RemovePlanField { pointer } => {
            validate_plan_pointer(pointer, false)?;
            let mut json = serde_json::to_value(&*plan).map_err(|source| {
                PatchApplicationError::Serialization {
                    context: "failed to serialise plan".to_owned(),
                    source,
                }
            })?;
            remove_json_pointer(&mut json, pointer)?;
            *plan =
                serde_json::from_value::<Plan>(json).map_err(PatchApplicationError::InvalidPlan)?;
        }
    }

    Ok(())
}

fn step_position(plan: &Plan, step_id: &str) -> Result<usize, PatchApplicationError> {
    plan.steps
        .iter()
        .position(|s| s.id == step_id)
        .ok_or_else(|| PatchApplicationError::StepNotFound {
            step_id: step_id.to_owned(),
        })
}

fn step_mut<'a>(
    plan: &'a mut Plan,
    step_id: &str,
) -> Result<&'a mut PlanStep, PatchApplicationError> {
    plan.steps
        .iter_mut()
        .find(|s| s.id == step_id)
        .ok_or_else(|| PatchApplicationError::StepNotFound {
            step_id: step_id.to_owned(),
        })
}

fn set_json_pointer(
    root: &mut serde_json::Value,
    pointer: &str,
    value: serde_json::Value,
) -> Result<(), PatchApplicationError> {
    if pointer.is_empty() {
        *root = value;
        return Ok(());
    }
    let Some(slot) = root.pointer_mut(pointer) else {
        return Err(invalid_pointer(format!(
            "JSON pointer '{pointer}' does not exist"
        )));
    };
    *slot = value;
    Ok(())
}

fn remove_json_pointer(
    root: &mut serde_json::Value,
    pointer: &str,
) -> Result<(), PatchApplicationError> {
    let (parent_pointer, token) = split_pointer(pointer)?;
    let parent = if parent_pointer.is_empty() {
        root
    } else {
        root.pointer_mut(&parent_pointer).ok_or_else(|| {
            invalid_pointer(format!(
                "JSON pointer parent '{parent_pointer}' does not exist"
            ))
        })?
    };

    match parent {
        serde_json::Value::Object(map) => map
            .remove(&token)
            .map(|_| ())
            .ok_or_else(|| invalid_pointer(format!("JSON pointer '{pointer}' does not exist"))),
        serde_json::Value::Array(items) => {
            let index = token.parse::<usize>().map_err(|_| {
                invalid_pointer(format!(
                    "JSON pointer '{pointer}' does not address an array index"
                ))
            })?;
            if index >= items.len() {
                return Err(invalid_pointer(format!(
                    "JSON pointer '{pointer}' does not exist"
                )));
            }
            items.remove(index);
            Ok(())
        }
        _ => Err(invalid_pointer(format!(
            "JSON pointer parent '{parent_pointer}' is not an object or array"
        ))),
    }
}

fn split_pointer(pointer: &str) -> Result<(String, String), PatchApplicationError> {
    if pointer.is_empty() {
        return Err(invalid_pointer("cannot remove the JSON root"));
    }
    if !pointer.starts_with('/') {
        return Err(invalid_pointer(format!(
            "JSON pointer '{pointer}' must be empty or start with '/'"
        )));
    }
    let Some((parent, raw_token)) = pointer.rsplit_once('/') else {
        return Err(invalid_pointer(format!("invalid JSON pointer '{pointer}'")));
    };
    Ok((parent.to_owned(), decode_pointer_token(raw_token)))
}

fn validate_plan_pointer(
    pointer: &str,
    reject_root_replacement: bool,
) -> Result<(), PatchApplicationError> {
    if reject_root_replacement && pointer.is_empty() {
        return Err(PatchApplicationError::PlanRootReplacement);
    }
    if pointer == "/metadata" || pointer.starts_with("/metadata/") {
        return Err(PatchApplicationError::ProtectedPlanMetadata {
            pointer: pointer.to_owned(),
        });
    }
    Ok(())
}

fn invalid_pointer(message: impl Into<String>) -> PatchApplicationError {
    PatchApplicationError::InvalidJsonPointer {
        message: message.into(),
    }
}

fn decode_pointer_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{PlanMetadata, PlanStep, StepConfig, ToolCallConfig};
    use crate::storage::patches::PatchOperation;
    use indexmap::IndexMap;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_step(id: &str) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "noop".to_owned(),
                arguments: IndexMap::new(),
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn make_plan(ids: &[&str]) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "test-plan".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: ids.iter().map(|id| make_step(id)).collect(),
            outputs: vec![],
        }
    }

    fn make_patch(failing_step_id: &str, operation: PatchOperation) -> Patch {
        Patch::new(
            "plan-1",
            1,
            "run-1",
            failing_step_id,
            operation,
            "rationale",
        )
    }

    fn step_ids(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.id.as_str()).collect()
    }

    // ── ReplaceStep ───────────────────────────────────────────────────────────

    #[test]
    fn replace_step_swaps_in_place() {
        let plan = make_plan(&["a", "b", "c"]);
        let mut replacement = make_step("b");
        replacement.name = "b-repaired".to_owned();
        let patch = make_patch(
            "b",
            PatchOperation::ReplaceStep {
                new_step: replacement,
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(step_ids(&result), ["a", "b", "c"]); // order preserved
        assert_eq!(result.steps[1].name, "b-repaired");
    }

    #[test]
    fn replace_step_first_position() {
        let plan = make_plan(&["a", "b"]);
        let replacement = make_step("a");
        let patch = make_patch(
            "a",
            PatchOperation::ReplaceStep {
                new_step: replacement,
            },
        );

        let result = apply_operation(plan, &patch).unwrap();
        assert_eq!(step_ids(&result), ["a", "b"]);
    }

    // ── UpdateStepConfig ──────────────────────────────────────────────────────

    #[test]
    fn update_step_config_changes_only_config() {
        let plan = make_plan(&["a", "b"]);
        let new_config = StepConfig::ToolCall(ToolCallConfig {
            tool: "new_tool".to_owned(),
            arguments: IndexMap::new(),
        });
        let patch = make_patch(
            "a",
            PatchOperation::UpdateStepConfig {
                new_config: new_config.clone(),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        // ID and name are unchanged; only config differs
        assert_eq!(result.steps[0].id, "a");
        assert_eq!(result.steps[0].name, "a");
        assert_eq!(result.steps[0].config, new_config);
    }

    // ── InsertBefore ──────────────────────────────────────────────────────────

    #[test]
    fn insert_before_pushes_target_down() {
        let plan = make_plan(&["a", "b"]);
        let new_step = make_step("pre-b");
        let patch = make_patch("b", PatchOperation::InsertBefore { step: new_step });

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.steps.len(), 3);
        assert_eq!(step_ids(&result), ["a", "pre-b", "b"]);
    }

    #[test]
    fn insert_before_first_step() {
        let plan = make_plan(&["a", "b"]);
        let new_step = make_step("pre-a");
        let patch = make_patch("a", PatchOperation::InsertBefore { step: new_step });

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(step_ids(&result), ["pre-a", "a", "b"]);
    }

    // ── InsertAfter ───────────────────────────────────────────────────────────

    #[test]
    fn insert_after_places_step_immediately_after_target() {
        let plan = make_plan(&["a", "b"]);
        let new_step = make_step("post-a");
        let patch = make_patch("a", PatchOperation::InsertAfter { step: new_step });

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.steps.len(), 3);
        assert_eq!(step_ids(&result), ["a", "post-a", "b"]);
    }

    #[test]
    fn insert_after_last_step_appends() {
        let plan = make_plan(&["a", "b"]);
        let new_step = make_step("post-b");
        let patch = make_patch("b", PatchOperation::InsertAfter { step: new_step });

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(step_ids(&result), ["a", "b", "post-b"]);
    }

    // ── JSON-pointer operations ───────────────────────────────────────────────

    #[test]
    fn set_step_field_updates_one_json_tree_value() {
        let plan = make_plan(&["a", "b"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetStepField {
                step_id: "b".to_owned(),
                pointer: "/name".to_owned(),
                value: serde_json::json!("renamed-b"),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.steps[0].name, "a");
        assert_eq!(result.steps[1].name, "renamed-b");
    }

    #[test]
    fn set_step_field_can_update_nested_config_values() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetStepField {
                step_id: "a".to_owned(),
                pointer: "/config/tool".to_owned(),
                value: serde_json::json!("http-get"),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        match &result.steps[0].config {
            StepConfig::ToolCall(cfg) => assert_eq!(cfg.tool, "http-get"),
            other => panic!("unexpected config: {other:?}"),
        }
    }

    #[test]
    fn batch_applies_operations_in_order() {
        let plan = make_plan(&["a", "b"]);
        let patch = make_patch(
            "a",
            PatchOperation::Batch {
                operations: vec![
                    PatchOperation::SetStepField {
                        step_id: "a".to_owned(),
                        pointer: "/name".to_owned(),
                        value: serde_json::json!("first"),
                    },
                    PatchOperation::SetStepField {
                        step_id: "b".to_owned(),
                        pointer: "/depends_on".to_owned(),
                        value: serde_json::json!(["a"]),
                    },
                ],
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.steps[0].name, "first");
        assert_eq!(result.steps[1].depends_on, vec!["a".to_owned()]);
    }

    #[test]
    fn empty_batch_is_rejected() {
        let plan = make_plan(&["a"]);
        let patch = make_patch("a", PatchOperation::Batch { operations: vec![] });

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(
            err.to_string().contains("at least one operation"),
            "got: {err}"
        );
    }

    #[test]
    fn batch_fails_as_a_whole_when_a_later_operation_fails() {
        // The caller receives Err and discards the plan, so a partially
        // applied batch is never observable — the whole batch must error.
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::Batch {
                operations: vec![
                    PatchOperation::SetStepField {
                        step_id: "a".to_owned(),
                        pointer: "/name".to_owned(),
                        value: serde_json::json!("renamed"),
                    },
                    PatchOperation::SetStepField {
                        step_id: "missing".to_owned(),
                        pointer: "/name".to_owned(),
                        value: serde_json::json!("x"),
                    },
                ],
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn set_step_field_same_value_is_a_no_op() {
        let plan = make_plan(&["a"]);
        let original = plan.steps[0].clone();
        let patch = make_patch(
            "a",
            PatchOperation::SetStepField {
                step_id: "a".to_owned(),
                pointer: "/name".to_owned(),
                value: serde_json::json!("a"),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.steps[0], original);
    }

    #[test]
    fn set_step_field_nonexistent_pointer_returns_err() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetStepField {
                step_id: "a".to_owned(),
                pointer: "/no_such_field".to_owned(),
                value: serde_json::json!(1),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn set_step_field_that_breaks_the_step_shape_returns_err() {
        // The pointer resolves, but the resulting JSON no longer
        // deserialises into a PlanStep — the patch must be rejected.
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetStepField {
                step_id: "a".to_owned(),
                pointer: "/id".to_owned(),
                value: serde_json::json!(42),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(
            err.to_string().contains("invalid after patch"),
            "got: {err}"
        );
    }

    #[test]
    fn remove_step_field_nonexistent_pointer_returns_err() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::RemoveStepField {
                step_id: "a".to_owned(),
                pointer: "/no_such_field".to_owned(),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn remove_step_field_can_remove_an_array_element() {
        let mut plan = make_plan(&["a", "b", "c"]);
        plan.steps[2].depends_on = vec!["a".to_owned(), "b".to_owned()];
        let patch = make_patch(
            "c",
            PatchOperation::RemoveStepField {
                step_id: "c".to_owned(),
                pointer: "/depends_on/0".to_owned(),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.steps[2].depends_on, vec!["b".to_owned()]);
    }

    #[test]
    fn remove_array_element_out_of_range_returns_err() {
        let mut plan = make_plan(&["a", "b"]);
        plan.steps[1].depends_on = vec!["a".to_owned()];
        let patch = make_patch(
            "b",
            PatchOperation::RemoveStepField {
                step_id: "b".to_owned(),
                pointer: "/depends_on/5".to_owned(),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn remove_plan_field_root_pointer_is_rejected() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::RemovePlanField {
                pointer: String::new(),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(
            err.to_string().contains("cannot remove the JSON root"),
            "got: {err}"
        );
    }

    #[test]
    fn remove_plan_field_without_leading_slash_is_rejected() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::RemovePlanField {
                pointer: "config".to_owned(),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(
            err.to_string().contains("must be empty or start with '/'"),
            "got: {err}"
        );
    }

    #[test]
    fn remove_plan_field_decodes_escaped_pointer_tokens() {
        // RFC 6901: "~1" encodes "/" and "~0" encodes "~" in a key name.
        let mut plan = make_plan(&["a"]);
        plan.config
            .insert("a/b~c".to_owned(), serde_json::json!(true));
        let patch = make_patch(
            "a",
            PatchOperation::RemovePlanField {
                pointer: "/config/a~1b~0c".to_owned(),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert!(!result.config.contains_key("a/b~c"));
    }

    #[test]
    fn set_plan_field_updates_top_level_value() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetPlanField {
                pointer: "/name".to_owned(),
                value: serde_json::json!("renamed-plan"),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert_eq!(result.name, "renamed-plan");
    }

    #[test]
    fn set_plan_field_rejects_root_replacement() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetPlanField {
                pointer: String::new(),
                value: serde_json::to_value(&plan).unwrap(),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(matches!(err, PatchApplicationError::PlanRootReplacement));
    }

    #[test]
    fn plan_field_operations_reject_lifecycle_metadata() {
        let plan = make_plan(&["a"]);
        let set_patch = make_patch(
            "a",
            PatchOperation::SetPlanField {
                pointer: "/metadata/version".to_owned(),
                value: serde_json::json!(99),
            },
        );
        let remove_patch = make_patch(
            "a",
            PatchOperation::RemovePlanField {
                pointer: "/metadata".to_owned(),
            },
        );

        let set_error = apply_operation(plan.clone(), &set_patch).unwrap_err();
        let remove_error = apply_operation(plan, &remove_patch).unwrap_err();

        assert!(matches!(
            set_error,
            PatchApplicationError::ProtectedPlanMetadata { .. }
        ));
        assert!(matches!(
            remove_error,
            PatchApplicationError::ProtectedPlanMetadata { .. }
        ));
    }

    #[test]
    fn set_plan_field_that_breaks_the_plan_shape_returns_err() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "a",
            PatchOperation::SetPlanField {
                pointer: "/steps".to_owned(),
                value: serde_json::json!("not-an-array"),
            },
        );

        let err = apply_operation(plan, &patch).unwrap_err();

        assert!(
            err.to_string().contains("invalid after patch"),
            "got: {err}"
        );
    }

    #[test]
    fn remove_plan_field_removes_json_tree_value() {
        let mut plan = make_plan(&["a"]);
        plan.config
            .insert("temporary".to_owned(), serde_json::json!(true));
        let patch = make_patch(
            "a",
            PatchOperation::RemovePlanField {
                pointer: "/config/temporary".to_owned(),
            },
        );

        let result = apply_operation(plan, &patch).unwrap();

        assert!(!result.config.contains_key("temporary"));
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn replace_step_not_found_returns_err() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "missing",
            PatchOperation::ReplaceStep {
                new_step: make_step("x"),
            },
        );
        let err = apply_operation(plan, &patch).unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "error should name the missing ID: {err}"
        );
    }

    #[test]
    fn update_config_not_found_returns_err() {
        let plan = make_plan(&["a"]);
        let new_config = StepConfig::ToolCall(ToolCallConfig {
            tool: "x".to_owned(),
            arguments: IndexMap::new(),
        });
        let patch = make_patch("missing", PatchOperation::UpdateStepConfig { new_config });
        let err = apply_operation(plan, &patch).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn insert_before_not_found_returns_err() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "missing",
            PatchOperation::InsertBefore {
                step: make_step("new"),
            },
        );
        let err = apply_operation(plan, &patch).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn insert_after_not_found_returns_err() {
        let plan = make_plan(&["a"]);
        let patch = make_patch(
            "missing",
            PatchOperation::InsertAfter {
                step: make_step("new"),
            },
        );
        let err = apply_operation(plan, &patch).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }
}
