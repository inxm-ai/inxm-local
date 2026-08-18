Build a reusable development workflow for this repository. Accept a plan
prompt, explicit expectations for the generated plan, a branch name, and the
repository root directory. Abort unless the checkout is clean, switch to main,
pull the latest changes with a fast-forward-only pull and stop without changing
the worktree if that fails, then create the requested branch.

Start this application's local MCP server and exercise the supplied plan prompt
end to end through the MCP `compile_plan` and `show_plan` tools.
Deterministically compare the generated plan with every supplied expectation.
If it already matches, ask the human which real-world behavior still fails and
use that feedback as an additional expectation.

Diagnose why compilation diverges and add a focused ignored
`live_spec_planning` regression test before changing implementation. Ask the
human to approve starting agent-driven remediation, stating the diagnosis and
the regression test as evidence; only after approval, use Codex or Claude
Code to fix the compiler and rerun the focused live test until all
expectations pass, with a bounded iteration limit and preserved failure
evidence. This AGENT_CALL requires the experimental agent-steps setting to be
enabled and an authenticated Codex or Claude Code account selected; abort
with that requirement if it is not.

Apply the `principal-developer` skill (`skills/principal-developer/SKILL.md`)
as a review gate, then run `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --all-targets --all-features`,
and all ignored `live_spec_planning` tests, fixing findings and repeating the
relevant checks until clean. Confirm the diff carries no credentials,
personal data, proprietary third-party code, or license-incompatible
generated material. Prepare a pull-request title and body describing the
change, why it is needed, how it was tested, and any follow-up work, ask the
human to approve publication, and only after approval push and create the
pull request with `gh` CLI.
