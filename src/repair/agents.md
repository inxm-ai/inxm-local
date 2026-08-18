# repair module

Propose and apply repairs for failed runs. A failure has two possible causes
and the diagnosis decides which: the plan is wrong → a constrained `Patch`;
the world is wrong (runtime state violated a reasonable plan's assumptions,
e.g. a commit step with nothing to commit) → a `WorldFix` with human
remediation actions, plan untouched, run resumable at the same version.
`propose_repair` calls the compiler backend. `apply_patch` is deterministic:
applies the op, re-validates, saves a new plan version. Human approval is a
CLI step, not part of this module.

Files:
- `classifier.rs` — error kind classification
- `failure_packet.rs` — failure context sent to the compiler
- `patch.rs` — patch apply logic

You own this dir only. Read other `src/` dirs for context, never edit them.
