Build a reusable test-first bugfix workflow for this repository. Accept a bug
report, reproduction expectations, a branch name, and the repository root
directory. Abort unless the checkout is clean, switch to main,
fast-forward-only pull the latest changes and stop without changing the
worktree if that fails, then create the branch.

Inspect the owning module and reproduce the bug with the smallest failing test
before editing production code. Diagnose the failure from evidence, then ask
the human to approve starting agent-driven remediation, stating the diagnosis
and the failing test as evidence. Only after approval, use Codex or Claude
Code to implement the fix in the appropriate module and rerun the focused
test until it passes. This AGENT_CALL requires the experimental agent-steps
setting to be enabled and an authenticated Codex or Claude Code account
selected; abort with that requirement if it is not.

Then apply the `principal-developer` skill
(`skills/principal-developer/SKILL.md`) as a review gate, run `cargo fmt
--check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo
test --all-targets --all-features`, and all ignored `live_spec_planning`
tests, fixing findings and repeating relevant checks until clean. Confirm the
diff carries no credentials, personal data, proprietary third-party code, or
license-incompatible generated material. Prepare a pull-request title and
body with the regression evidence, why the fix is needed, and any follow-up
work, ask the human to approve publication, and only after approval push and
create the pull request with `gh` CLI.
