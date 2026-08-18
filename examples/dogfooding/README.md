# Repository dogfooding workflows

These prompts are acceptance-tested against the live Codex compiler in
`tests/live_spec_planning.rs`. Paste one into the application to compile a
draft plan, review the generated design, and publish it only after it matches
the live contract.

- `prompt-to-plan.md` improves this repository's prompt-to-plan behavior by
  reproducing a mismatch through the local MCP server and adding a live
  regression before fixing compiler code.
- `feature-development.md` decomposes an approved feature into module-owned
  packages, implements independent packages concurrently, and gates GitHub
  publication on human approval.
- `bugfix.md` reproduces a reported bug with the smallest failing test before
  changing production code.

All workflows require an explicit `root_directory`, refuse to proceed from a
dirty checkout, use fast-forward-only pulls, apply the `principal-developer`
skill (`skills/principal-developer/SKILL.md`) as their review gate, run the
exact quality gates from `CONTRIBUTING.md` (`cargo fmt --check`, `cargo
clippy --all-targets --all-features -- -D warnings`, `cargo test
--all-targets --all-features`, plus all ignored `live_spec_planning` tests),
confirm the diff carries no credentials, personal data, proprietary
third-party code, or license-incompatible generated material, and keep `gh pr
create` behind a human approval step.

## Experimental agent steps must be turned on first

Every workflow's actual implementation/fix/remediation work runs through an
`AGENT_CALL` step — a real, tool-using Codex or Claude Code CLI with
workspace-write access, not a fixed script. That step type only exists behind
the app's experimental toggle: open **Settings → Experimental execution →
Enable experimental agent steps**, and select an OpenAI (Codex) account,
Claude account, or an explicitly agent-shaped Custom CLI as the backend. If
the toggle is off:
- Pasting one of these prompts into the compiler will not produce an
  `AGENT_CALL` step at all — the compiler is told AGENT_CALL is unavailable
  and must fall back to deterministic steps, which cannot do open-ended
  coding-agent work.
- Importing one of the checked-in `*.plan.json` bundles is refused outright
  with an explicit error naming the missing setting.

This is deliberate, not a bug to work around: `AGENT_CALL` can run arbitrary
commands and write arbitrary files, so the app requires an explicit,
per-install opt-in before any plan can use it. Each workflow also adds its
own second, narrower gate on top of that system setting: a
`HUMAN_INTERACTION` approval step sits immediately before the AGENT_CALL step
that actually edits the workspace (`approve_remediation` in the bugfix and
prompt-to-plan bundles; approval of the package plan in
feature-development), separate from the later approval before `gh pr
create`. A step that only inspects the repository or reproduces a bug
without editing production code (e.g. `create_regression_test`) may run as
an `AGENT_CALL` before that gate, since it has no write side effect yet — see
`assert_agent_call_is_used_and_approval_gated` in `tests/live_spec_planning.rs`
for the exact invariant this is checked against.

Driving one of these workflows to a human-approved, `CONTRIBUTING.md`-clean
pull request is a recognized way to contribute to this repository — it is not
just a self-test of the compiler. The workflow output is a normal PR and goes
through normal review; nothing here bypasses the CLA or PR process described
in `CONTRIBUTING.md`.

Validated, importable plan bundles live beside the source prompts as
`*.plan.json`. Refresh all three through the authenticated MCP `compile_plan`,
`show_plan`, and `export_plan` tools with:

```sh
cargo run --example export_dogfooding
```

Pass one or more workflow slugs to refresh selected bundles, for example
`cargo run --example export_dogfooding -- bugfix`.

Retry loops must be finite. A compiled plan should generate a bounded attempt
list and use `FAN_OUT.until` with a deterministic verifier; ordinary map-style
fan-outs omit `until`.
