//! Integration tests: plan round-trip, validation, and execution.

use inxm_local::error::ValidationErrorKind;
use inxm_local::plan;
use inxm_local::plan::bundle::{CURRENT_FORMAT_VERSION, PlanBundle};
use inxm_local::plan::normalization::normalize;
use inxm_local::plan::types::{Plan, PlanMetadata, PlanStep, StepConfig, ToolCallConfig};
use inxm_local::tools::catalog::ToolCatalog;
use inxm_local::validator;
use std::path::Path;

// ─── Plan round-trip ─────────────────────────────────────────────────────────

#[test]
fn valid_plan_loads_and_serialises() {
    let path = Path::new("tests/fixtures/plans/valid_plan.json");
    let plan = plan::load_from_file(path).expect("should load valid plan");
    assert_eq!(plan.name, "Hello World Plan");
    assert_eq!(plan.steps.len(), 2);

    // Serialise and re-parse — must be identical
    let json = plan::to_json(&plan).expect("should serialise");
    let reparsed = plan::from_json(&json).expect("should re-parse");
    assert_eq!(plan, reparsed);
}

#[test]
fn checked_in_dogfooding_bundles_are_importable() {
    for slug in ["prompt-to-plan", "feature-development", "bugfix"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/dogfooding")
            .join(format!("{slug}.plan.json"));
        let bundle = PlanBundle::load_from_file(&path)
            .unwrap_or_else(|error| panic!("could not import {}: {error}", path.display()));
        assert_eq!(bundle.format_version, CURRENT_FORMAT_VERSION);
        assert!(
            !bundle.plan.steps.is_empty(),
            "{} contains an empty plan",
            path.display()
        );
    }
}

#[test]
fn cycle_plan_fails_validation() {
    let path = Path::new("tests/fixtures/plans/invalid_cycle.json");
    let plan = plan::load_from_file(path).expect("should load cycle plan");

    let catalog = ToolCatalog::load_from_file(Path::new("tests/fixtures/tools/catalog.yaml"))
        .expect("catalog should load");

    let errors = validator::validate(&plan, &catalog);
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ValidationErrorKind::CyclicDependency),
        "expected CyclicDependency error, got: {errors:?}"
    );
}

#[test]
fn valid_plan_passes_validation() {
    let path = Path::new("tests/fixtures/plans/valid_plan.json");
    let plan = plan::load_from_file(path).expect("should load valid plan");

    let catalog = ToolCatalog::load_from_file(Path::new("tests/fixtures/tools/catalog.yaml"))
        .expect("catalog should load");

    let errors = validator::validate(&plan, &catalog);
    assert!(
        errors.is_empty(),
        "unexpected validation errors: {errors:?}"
    );
}

// ─── Normalization ───────────────────────────────────────────────────────────

#[test]
fn normalization_is_idempotent_on_valid_plan() {
    let path = Path::new("tests/fixtures/plans/valid_plan.json");
    let plan = plan::load_from_file(path).expect("should load");

    let once = normalize(plan.clone());
    let twice = normalize(once.clone());

    let ids_once: Vec<&str> = once.steps.iter().map(|s| s.id.as_str()).collect();
    let ids_twice: Vec<&str> = twice.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids_once, ids_twice, "normalization should be idempotent");
}

#[test]
fn normalization_sorts_dependencies_in_alphabetical_order() {
    let step = PlanStep {
        id: "c".to_owned(),
        name: "c".to_owned(),
        description: None,
        config: StepConfig::ToolCall(ToolCallConfig {
            tool: "echo".to_owned(),
            arguments: Default::default(),
        }),
        depends_on: vec!["z".to_owned(), "a".to_owned(), "m".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };

    let mut plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "t".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![
            PlanStep {
                id: "a".to_owned(),
                name: "a".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "echo".to_owned(),
                    arguments: Default::default(),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
            PlanStep {
                id: "m".to_owned(),
                name: "m".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "echo".to_owned(),
                    arguments: Default::default(),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
            PlanStep {
                id: "z".to_owned(),
                name: "z".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "echo".to_owned(),
                    arguments: Default::default(),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
            step,
        ],
        outputs: vec![],
    };

    plan = normalize(plan);
    let c_step = plan.steps.iter().find(|s| s.id == "c").unwrap();
    assert_eq!(c_step.depends_on, ["a", "m", "z"]);
}

// ─── Storage round-trip ──────────────────────────────────────────────────────

#[test]
fn plan_storage_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = inxm_local::storage::StorageRoot::open(dir.path()).expect("storage open");

    let path = Path::new("tests/fixtures/plans/valid_plan.json");
    let plan = plan::load_from_file(path).expect("load");

    storage.plans().save(&plan).expect("save");

    let loaded = storage
        .plans()
        .load_current(&plan.metadata.id)
        .expect("load_current");
    assert_eq!(plan.name, loaded.name);
    assert_eq!(plan.metadata.version, loaded.metadata.version);
}

#[test]
fn plan_versioning_stores_multiple_versions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = inxm_local::storage::StorageRoot::open(dir.path()).expect("storage open");

    let path = Path::new("tests/fixtures/plans/valid_plan.json");
    let plan_v1 = plan::load_from_file(path).expect("load");

    // Save version 1
    storage.plans().save(&plan_v1).expect("save v1");

    // Create version 2
    let mut plan_v2 = plan_v1.clone();
    plan_v2.metadata = plan_v2.metadata.next_version();
    storage.plans().save(&plan_v2).expect("save v2");

    let versions = storage
        .plans()
        .list_versions(&plan_v1.metadata.id)
        .expect("list");
    assert_eq!(versions, [1, 2]);

    let current = storage
        .plans()
        .load_current(&plan_v1.metadata.id)
        .expect("current");
    assert_eq!(current.metadata.version, 2);
}
