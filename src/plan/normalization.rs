//! Deterministic plan normalization passes.
//!
//! Normalization must be idempotent: running it twice produces the same result.
//! It runs after compilation and after every patch application.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use super::types::{Plan, PlanStep};

/// Apply all normalization passes to a plan and return the result.
///
/// Passes:
/// 1. Sort `depends_on` lists alphabetically inside each step.
/// 2. Sort steps into stable topological order (dependencies first, ties broken alphabetically).
/// 3. Sort `config` keys alphabetically.
pub fn normalize(mut plan: Plan) -> Plan {
    for step in &mut plan.steps {
        step.depends_on.sort();
    }
    plan.steps = normalize_step_order(plan.steps);
    plan.config = sorted_by_key(plan.config);
    plan
}

/// Re-order a map's entries alphabetically by key, for stable JSON diffs.
fn sorted_by_key(map: IndexMap<String, serde_json::Value>) -> IndexMap<String, serde_json::Value> {
    let mut pairs: Vec<_> = map.into_iter().collect();
    pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
    pairs.into_iter().collect()
}

/// Return steps sorted in stable topological order.
///
/// Kahn's algorithm, layer by layer: each round collects every step whose
/// in-plan dependencies are all already ordered, and appends that layer in
/// alphabetical order by step ID. This makes plan JSON diffs readable and
/// deterministic.
///
/// Normalization must stay total even for plans the validator will reject:
/// - Dependencies on step IDs that don't exist in the plan are ignored here
///   (the validator reports them as errors).
/// - If a dependency cycle prevents progress, the remaining steps are
///   appended in alphabetical order so no step is dropped (the validator
///   reports the cycle).
/// - Duplicate step IDs remain distinct so the validator can reject them.
fn normalize_step_order(steps: Vec<PlanStep>) -> Vec<PlanStep> {
    let mut indices_by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, step) in steps.iter().enumerate() {
        indices_by_id
            .entry(step.id.as_str())
            .or_default()
            .push(index);
    }

    // Each vector position represents one concrete step instance. A dependency
    // on a duplicated ID waits for every matching instance; the validator will
    // then report the ambiguous duplicate without normalization losing data.
    let mut pending_deps: Vec<Option<HashSet<usize>>> = steps
        .iter()
        .map(|step| {
            Some(
                step.depends_on
                    .iter()
                    .filter_map(|dependency| indices_by_id.get(dependency.as_str()))
                    .flatten()
                    .copied()
                    .collect(),
            )
        })
        .collect();

    let mut ordered_indices = Vec::with_capacity(steps.len());
    while ordered_indices.len() < steps.len() {
        let mut ready: Vec<usize> = pending_deps
            .iter()
            .enumerate()
            .filter_map(|(index, dependencies)| {
                dependencies
                    .as_ref()
                    .is_some_and(HashSet::is_empty)
                    .then_some(index)
            })
            .collect();
        if ready.is_empty() {
            // Cycle detected — append the cyclic remainder alphabetically so
            // the output is still deterministic and no step is lost.
            let mut leftover: Vec<usize> = pending_deps
                .iter()
                .enumerate()
                .filter_map(|(index, dependencies)| dependencies.as_ref().map(|_| index))
                .collect();
            sort_step_indices(&mut leftover, &steps);
            ordered_indices.extend(leftover);
            break;
        }
        sort_step_indices(&mut ready, &steps);

        for &index in &ready {
            pending_deps[index] = None;
        }
        for dependencies in pending_deps.iter_mut().flatten() {
            for ready_index in &ready {
                dependencies.remove(ready_index);
            }
        }
        ordered_indices.extend(ready);
    }

    let mut steps: Vec<Option<PlanStep>> = steps.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(ordered_indices.len());
    for index in ordered_indices {
        // Every index is emitted once: ready entries are removed from
        // `pending_deps`, while the cycle fallback emits only remaining entries.
        ordered.push(steps[index].take().expect("step index is emitted once"));
    }
    ordered
}

fn sort_step_indices(indices: &mut [usize], steps: &[PlanStep]) {
    indices.sort_by(|left, right| {
        steps[*left]
            .id
            .cmp(&steps[*right].id)
            .then_with(|| left.cmp(right))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{PlanMetadata, StepConfig, ToolCallConfig};

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

    fn step_ids(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn single_step_roundtrips() {
        let plan = make_plan(vec![make_step("a", vec![])]);
        let normalized = normalize(plan.clone());
        assert_eq!(normalized.steps[0].id, "a");
    }

    #[test]
    fn sorts_independent_steps_alphabetically() {
        let plan = make_plan(vec![
            make_step("c", vec![]),
            make_step("a", vec![]),
            make_step("b", vec![]),
        ]);
        let normalized = normalize(plan);
        assert_eq!(step_ids(&normalized), ["a", "b", "c"]);
    }

    #[test]
    fn dependencies_come_before_dependents() {
        let plan = make_plan(vec![
            make_step("c", vec!["a", "b"]),
            make_step("b", vec!["a"]),
            make_step("a", vec![]),
        ]);
        let normalized = normalize(plan);
        assert_eq!(step_ids(&normalized), ["a", "b", "c"]);
    }

    #[test]
    fn dependency_order_wins_over_alphabetical_order() {
        // "z" is a dependency of "a", so it must come first despite sorting
        // last alphabetically.
        let plan = make_plan(vec![make_step("a", vec!["z"]), make_step("z", vec![])]);
        let normalized = normalize(plan);
        assert_eq!(step_ids(&normalized), ["z", "a"]);
    }

    #[test]
    fn unknown_dependencies_do_not_block_ordering() {
        // The validator rejects unknown dependency IDs; normalization must
        // still order such plans deterministically without dropping steps.
        let plan = make_plan(vec![make_step("b", vec!["ghost"]), make_step("a", vec![])]);
        let normalized = normalize(plan);
        assert_eq!(step_ids(&normalized), ["a", "b"]);
        assert_eq!(normalized.steps[1].depends_on, ["ghost"]);
    }

    #[test]
    fn cyclic_steps_are_kept_in_deterministic_order() {
        // The validator rejects cycles; normalization must stay total —
        // acyclic steps are ordered first, the cyclic remainder is appended
        // alphabetically, and nothing is dropped.
        let plan = make_plan(vec![
            make_step("b", vec!["a"]),
            make_step("a", vec!["b"]),
            make_step("c", vec![]),
        ]);
        let once = normalize(plan);
        assert_eq!(step_ids(&once), ["c", "a", "b"]);

        let twice = normalize(once.clone());
        assert_eq!(once, twice, "cycle fallback must stay idempotent");
    }

    #[test]
    fn duplicate_ids_are_preserved_for_validation() {
        let mut first = make_step("duplicate", vec![]);
        first.name = "first".to_owned();
        let mut second = make_step("duplicate", vec![]);
        second.name = "second".to_owned();
        let dependent = make_step("dependent", vec!["duplicate"]);

        let normalized = normalize(make_plan(vec![dependent, second, first]));

        assert_eq!(normalized.steps.len(), 3);
        assert_eq!(
            normalized
                .steps
                .iter()
                .map(|step| (step.id.as_str(), step.name.as_str()))
                .collect::<Vec<_>>(),
            [
                ("duplicate", "second"),
                ("duplicate", "first"),
                ("dependent", "dependent"),
            ]
        );
    }

    #[test]
    fn malformed_graph_normalization_preserves_content_and_is_idempotent() {
        let cases = vec![
            vec![
                make_step("duplicate", vec!["missing"]),
                make_step("duplicate", vec![]),
                make_step("dependent", vec!["duplicate", "duplicate"]),
            ],
            vec![
                make_step("b", vec!["a"]),
                make_step("a", vec!["b"]),
                make_step("a", vec!["missing"]),
            ],
        ];

        for steps in cases {
            let expected_count = steps.len();
            let mut expected_steps = steps.clone();
            for step in &mut expected_steps {
                step.depends_on.sort();
            }

            let once = normalize(make_plan(steps));
            let twice = normalize(once.clone());
            assert_eq!(once, twice, "malformed normalization must be idempotent");
            assert_eq!(once.steps.len(), expected_count);

            for expected in expected_steps {
                assert!(
                    once.steps.contains(&expected),
                    "normalization must preserve every malformed step: {expected:?}"
                );
            }
        }
    }

    #[test]
    fn sorts_config_keys_alphabetically() {
        let mut plan = make_plan(vec![make_step("a", vec![])]);
        plan.config = [
            ("zeta".to_owned(), serde_json::json!(1)),
            ("alpha".to_owned(), serde_json::json!({"nested": true})),
            ("mid".to_owned(), serde_json::json!("v")),
        ]
        .into_iter()
        .collect();

        let normalized = normalize(plan);
        let keys: Vec<_> = normalized.config.keys().map(String::as_str).collect();
        assert_eq!(keys, ["alpha", "mid", "zeta"]);
        assert_eq!(
            normalized.config["alpha"],
            serde_json::json!({"nested": true})
        );
    }

    /// Normalization must not special-case
    /// `root_directory` — an optional input with no default (meaning "use
    /// the app-managed scratch workspace") must survive untouched: not
    /// dropped, not flipped to required, and no default invented for it.
    #[test]
    fn optional_root_directory_input_survives_normalization_unchanged() {
        use crate::plan::types::{PlanInput, ROOT_DIRECTORY_INPUT};

        let mut plan = make_plan(vec![make_step("a", vec![])]);
        plan.inputs = vec![PlanInput {
            name: ROOT_DIRECTORY_INPUT.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: false,
            default: None,
            input_kind: crate::plan::types::InputKind::Value,
        }];

        let normalized = normalize(plan.clone());
        assert_eq!(normalized.inputs, plan.inputs);
        assert_eq!(normalized.inputs.len(), 1);
        assert!(!normalized.inputs[0].required);
        assert!(normalized.inputs[0].default.is_none());
    }

    #[test]
    fn normalize_is_idempotent() {
        let mut plan = make_plan(vec![
            make_step("c", vec!["a"]),
            make_step("b", vec![]),
            make_step("a", vec![]),
        ]);
        plan.config = [
            ("z".to_owned(), serde_json::json!(1)),
            ("a".to_owned(), serde_json::json!(2)),
        ]
        .into_iter()
        .collect();
        plan.steps[0].depends_on.push("b".to_owned());

        let once = normalize(plan);
        let twice = normalize(once.clone());
        assert_eq!(
            once, twice,
            "normalize(normalize(p)) must equal normalize(p)"
        );
    }
}
