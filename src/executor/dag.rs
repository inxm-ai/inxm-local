//! DAG utilities for plan execution ordering.
//!
//! Uses Kahn's algorithm so that steps at the same topological level
//! are always returned in stable alphabetical order, making execution
//! deterministic and plan diffs readable.

use crate::plan::types::{Plan, PlanStep};
use std::collections::HashSet;

/// Return plan steps in topological order suitable for execution.
///
/// Independent steps at the same level are returned in alphabetical order
/// by step ID.  If the plan contains a cycle the remaining steps are
/// appended in alphabetical order (the validator should have caught this).
pub fn topological_order(plan: &Plan) -> Vec<&PlanStep> {
    let n = plan.steps.len();
    if n == 0 {
        return vec![];
    }

    // Map step_id → position in plan.steps for fast look-up.
    let id_to_idx: std::collections::HashMap<&str, usize> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();

    // in_degree[i] = number of plan-internal dependencies for step i.
    let mut in_degree: Vec<usize> = plan
        .steps
        .iter()
        .map(|s| {
            s.depends_on
                .iter()
                .filter(|dep| id_to_idx.contains_key(dep.as_str()))
                .count()
        })
        .collect();

    // Seed the queue with all zero-degree steps, alphabetically sorted.
    let mut queue: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    queue.sort_by_key(|&i| plan.steps[i].id.as_str());

    let mut result: Vec<&PlanStep> = Vec::with_capacity(n);
    let mut emitted = vec![false; n];

    while !queue.is_empty() {
        let idx = queue.remove(0);
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        result.push(&plan.steps[idx]);

        let step_id = &plan.steps[idx].id;

        // Decrement in-degree for every step that lists this one as a dep.
        for (i, step) in plan.steps.iter().enumerate() {
            if !emitted[i] && step.depends_on.contains(step_id) {
                in_degree[i] = in_degree[i].saturating_sub(1);
                if in_degree[i] == 0 {
                    queue.push(i);
                }
            }
        }
        // Keep the queue alphabetically sorted after each merge so ties at
        // the same topological level are emitted deterministically.
        queue.sort_by_key(|&i| plan.steps[i].id.as_str());
    }

    // Cycle fallback: append any un-emitted steps alphabetically.
    let mut remaining: Vec<usize> = (0..n).filter(|&i| !emitted[i]).collect();
    remaining.sort_by_key(|&i| plan.steps[i].id.as_str());
    for i in remaining {
        result.push(&plan.steps[i]);
    }

    result
}

/// Return `step_id` plus every step transitively downstream of it (via
/// `depends_on` edges), recomputed fresh from `plan`.
///
/// Used by repair-resume to decide which already-recorded `StepRun`s must be
/// reset to `Pending`. Always recompute from the current plan rather than an
/// older run's edges: a repair patch can insert, replace, or rewire steps, so
/// "downstream" only means anything relative to the plan being resumed
/// against.
///
/// If `step_id` no longer exists in `plan` (e.g. a patch replaced it under a
/// new ID) the result is empty — there is nothing in the current graph to
/// reset, and any replacement step starts fresh anyway since it has no prior
/// `StepRun` entry.
pub fn downstream_closure(plan: &Plan, step_id: &str) -> HashSet<String> {
    let mut reachable: HashSet<String> = HashSet::new();
    if plan.step(step_id).is_none() {
        return reachable;
    }
    reachable.insert(step_id.to_owned());

    let mut changed = true;
    while changed {
        changed = false;
        for step in &plan.steps {
            if reachable.contains(&step.id) {
                continue;
            }
            if step.depends_on.iter().any(|dep| reachable.contains(dep)) {
                reachable.insert(step.id.clone());
                changed = true;
            }
        }
    }
    reachable
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{PlanMetadata, PlanStep, StepConfig, ToolCallConfig};
    use indexmap::IndexMap;

    fn make_step(id: &str, depends_on: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "noop".to_owned(),
                arguments: IndexMap::new(),
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
            config: IndexMap::new(),
            steps,
            outputs: vec![],
        }
    }

    // ── topological_order ─────────────────────────────────────────────────────

    #[test]
    fn empty_plan_returns_empty_order() {
        let plan = make_plan(vec![]);
        assert!(topological_order(&plan).is_empty());
    }

    #[test]
    fn single_step_is_returned() {
        let plan = make_plan(vec![make_step("a", vec![])]);
        let order = topological_order(&plan);
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].id, "a");
    }

    #[test]
    fn dependency_comes_before_dependent() {
        let plan = make_plan(vec![make_step("b", vec!["a"]), make_step("a", vec![])]);
        let order = topological_order(&plan);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        let a = ids.iter().position(|&x| x == "a").unwrap();
        let b = ids.iter().position(|&x| x == "b").unwrap();
        assert!(a < b, "a must come before b");
    }

    #[test]
    fn chain_of_three_ordered_correctly() {
        let plan = make_plan(vec![
            make_step("c", vec!["b"]),
            make_step("b", vec!["a"]),
            make_step("a", vec![]),
        ]);
        let order = topological_order(&plan);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn independent_steps_sorted_alphabetically() {
        let plan = make_plan(vec![
            make_step("c", vec![]),
            make_step("a", vec![]),
            make_step("b", vec![]),
        ]);
        let order = topological_order(&plan);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn diamond_topology_respected() {
        // a → b, a → c, b + c → d
        let plan = make_plan(vec![
            make_step("d", vec!["b", "c"]),
            make_step("c", vec!["a"]),
            make_step("b", vec!["a"]),
            make_step("a", vec![]),
        ]);
        let order = topological_order(&plan);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        let a = ids.iter().position(|&x| x == "a").unwrap();
        let b = ids.iter().position(|&x| x == "b").unwrap();
        let c = ids.iter().position(|&x| x == "c").unwrap();
        let d = ids.iter().position(|&x| x == "d").unwrap();
        assert!(a < b);
        assert!(a < c);
        assert!(b < d);
        assert!(c < d);
    }

    #[test]
    fn cycle_members_are_appended_alphabetically_after_acyclic_steps() {
        // a ⇄ b form a cycle (the validator should reject this upstream);
        // the executor must still produce a deterministic total order rather
        // than dropping or looping on the cyclic steps.
        let plan = make_plan(vec![
            make_step("b", vec!["a"]),
            make_step("a", vec!["b"]),
            make_step("standalone", vec![]),
        ]);
        let order = topological_order(&plan);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["standalone", "a", "b"]);
    }

    // ── downstream_closure ────────────────────────────────────────────────────

    #[test]
    fn downstream_closure_includes_the_step_itself() {
        let plan = make_plan(vec![make_step("a", vec![])]);
        let closure = downstream_closure(&plan, "a");
        assert_eq!(closure, ["a".to_owned()].into_iter().collect());
    }

    #[test]
    fn downstream_closure_follows_chain() {
        let plan = make_plan(vec![
            make_step("a", vec![]),
            make_step("b", vec!["a"]),
            make_step("c", vec!["b"]),
        ]);
        let closure = downstream_closure(&plan, "a");
        assert_eq!(
            closure,
            ["a", "b", "c"].into_iter().map(str::to_owned).collect()
        );
    }

    #[test]
    fn downstream_closure_excludes_unrelated_siblings() {
        let plan = make_plan(vec![
            make_step("a", vec![]),
            make_step("b", vec!["a"]),
            make_step("sibling", vec![]),
        ]);
        let closure = downstream_closure(&plan, "a");
        assert_eq!(closure, ["a", "b"].into_iter().map(str::to_owned).collect());
        assert!(!closure.contains("sibling"));
    }

    #[test]
    fn downstream_closure_excludes_upstream_dependencies() {
        let plan = make_plan(vec![make_step("a", vec![]), make_step("b", vec!["a"])]);
        let closure = downstream_closure(&plan, "b");
        assert_eq!(closure, ["b".to_owned()].into_iter().collect());
        assert!(!closure.contains("a"));
    }

    #[test]
    fn downstream_closure_is_empty_when_step_missing() {
        let plan = make_plan(vec![make_step("a", vec![])]);
        assert!(downstream_closure(&plan, "missing").is_empty());
    }

    #[test]
    fn downstream_closure_follows_diamond_topology() {
        // a -> b, a -> c, b + c -> d
        let plan = make_plan(vec![
            make_step("a", vec![]),
            make_step("b", vec!["a"]),
            make_step("c", vec!["a"]),
            make_step("d", vec!["b", "c"]),
        ]);
        let closure = downstream_closure(&plan, "a");
        assert_eq!(
            closure,
            ["a", "b", "c", "d"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }
}
