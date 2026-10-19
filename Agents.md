# Agents.md

Rules for every agent working in this repo.

## Scope

You own ONE module dir: `src/app`, `src/compiler`, `src/executor`, `src/plan`,
`src/repair`, `src/storage`, `src/tools`, or `src/validator`.

You may read any other dir to understand context. You may never edit files
outside your assigned module. Need functionality, send request to the user 
so they have it build and come back.

### Root integration owner

When the user explicitly assigns root integration ownership, that agent may edit only
`Agents.md`, `Cargo.toml`, `Cargo.lock`, root-level integration tests, README/docs, and CI
metadata. The root integration owner may read module sources to coordinate public contracts,
but must never edit `src/**`; module source changes remain with the corresponding module owner.

### Delegated desktop delivery ownership

For the user-authorized executable discovery and desktop experience delivery, the
root integration owner may assign separate agents to `src/app`, `src/storage`,
and shared startup/packaging (`src/main.rs`, `src/hostenv.rs`, `build.rs`, and
platform packaging scripts). Each delegated agent edits only its assigned scope.
The root integration owner retains the root-only scope above. Cross-module
changes must be requested from the corresponding owner.

The app coordinator may delegate `src/app/updater.rs` exclusively to an updater
backend owner; the coordinator owns the remaining app files and public wiring.
After completing startup work, the shared startup owner may additionally own
`src/app/documentation.rs` exclusively; the app coordinator retains its wiring.

## No directory given?

Stop. Ask the user which module you own. Do not guess. Do not explore the
repo first.

## Minimal context

You start with almost no context on purpose. Do not go explore the codebase
to figure out the task. Ask the user instead.

Read only your module's `agents.md`, then ask the user what to do.

## Module docs

Each module dir has a short `agents.md`. Read it before doing anything else.
