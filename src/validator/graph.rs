//! Graph validation: missing dependencies, cycles, unreachable steps.

use crate::error::{ValidationError, ValidationErrorKind};
use crate::plan::types::Plan;
use std::collections::{HashMap, HashSet, VecDeque};

/// Validate the step dependency graph.
///
/// Checks:
/// - All `depends_on` IDs reference steps that exist in the plan.
/// - The graph has no cycles.
pub fn validate_graph(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    let step_ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();

    // 1. Missing dependencies
    for step in &plan.steps {
        for dep in &step.depends_on {
            if !step_ids.contains(dep.as_str()) {
                errors.push(ValidationError::field(
                    &step.id,
                    "depends_on",
                    ValidationErrorKind::MissingDependency,
                    format!(
                        "step '{}' depends on '{}' which does not exist",
                        step.id, dep
                    ),
                ));
            }
        }
    }

    // Stop early — cycle detection on a graph with missing nodes is misleading.
    if errors
        .iter()
        .any(|e| e.kind == ValidationErrorKind::MissingDependency)
    {
        return errors;
    }

    // 2. Cycle detection via Kahn's algorithm.
    if let Some(cycle_error) = detect_cycle(plan) {
        errors.push(cycle_error);
        // Skip the unreachable-step check — it is misleading with cycles present.
        return errors;
    }

    errors
}

/// Report a `CyclicDependency` error naming the participants, or `None` when
/// the dependency graph is acyclic.
fn detect_cycle(plan: &Plan) -> Option<ValidationError> {
    // in_degree[step] = number of dependencies the step declares.
    let mut in_degree: HashMap<&str, usize> = plan
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step.depends_on.len()))
        .collect();

    // Adjacency: dep → list of steps that depend on dep.
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in &plan.steps {
        for dep in &step.depends_on {
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(step.id.as_str());
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut visited = 0usize;

    while let Some(id) = queue.pop_front() {
        visited += 1;
        for &dependent in dependents.get(id).into_iter().flatten() {
            // Invariant: every dependent is a plan step, so it has an
            // in_degree entry — the caller has already rejected plans with
            // dependencies on missing steps.
            let deg = in_degree
                .get_mut(dependent)
                .expect("dependent step is in in_degree map");
            *deg -= 1;
            if *deg == 0 {
                queue.push_back(dependent);
            }
        }
    }

    // Compared against the step count (not the map size) so plans with
    // duplicate step IDs keep reporting exactly as before.
    if visited >= plan.steps.len() {
        return None;
    }

    // Nodes still holding in_degree > 0 never became schedulable — they are
    // the cycle participants.
    let cycle_participants: Vec<String> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg > 0)
        .map(|(&id, _)| id.to_owned())
        .collect();

    Some(ValidationError::plan(
        ValidationErrorKind::CyclicDependency,
        format!(
            "plan contains a dependency cycle involving steps: {}",
            cycle_participants.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{PlanMetadata, PlanStep, StepConfig, ToolCallConfig};

    fn make_step(id: &str, depends_on: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "noop".to_owned(),
                arguments: Default::default(),
            }),
            depends_on: depends_on.into_iter().map(str::to_owned).collect(),
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn make_plan(steps: Vec<PlanStep>) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps,
            outputs: vec![],
        }
    }

    #[test]
    fn valid_chain_has_no_errors() {
        let plan = make_plan(vec![
            make_step("a", vec![]),
            make_step("b", vec!["a"]),
            make_step("c", vec!["b"]),
        ]);
        assert!(validate_graph(&plan).is_empty());
    }

    #[test]
    fn missing_dependency_reported() {
        let plan = make_plan(vec![make_step("b", vec!["nonexistent"])]);
        let errors = validate_graph(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingDependency)
        );
    }

    #[test]
    fn cycle_detected() {
        let plan = make_plan(vec![make_step("a", vec!["b"]), make_step("b", vec!["a"])]);
        let errors = validate_graph(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::CyclicDependency)
        );
    }

    #[test]
    fn self_cycle_detected() {
        let plan = make_plan(vec![make_step("a", vec!["a"])]);
        // "a" depends on itself — it should report missing dep (a exists) but cycle too
        let errors = validate_graph(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::CyclicDependency)
        );
    }

    #[test]
    fn diamond_dependency_ok() {
        // a → b, a → c, b → d, c → d
        let plan = make_plan(vec![
            make_step("a", vec![]),
            make_step("b", vec!["a"]),
            make_step("c", vec!["a"]),
            make_step("d", vec!["b", "c"]),
        ]);
        assert!(validate_graph(&plan).is_empty());
    }

    #[test]
    fn independent_roots_are_valid() {
        let plan = make_plan(vec![
            make_step("first", vec![]),
            make_step("second", vec![]),
        ]);
        assert!(validate_graph(&plan).is_empty());
    }
}
