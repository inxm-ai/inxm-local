Build a reusable feature-development workflow for this repository. Accept a
feature request, acceptance expectations, a branch name, and the repository
root directory. Inspect the request and repository to identify clean
module-owned implementation packages, propose a plan to the human, and revise
it from their feedback until they explicitly approve it.

Abort unless the checkout is clean, switch to main, pull latest with
fast-forward-only semantics and stop without modifying the worktree on
conflicts, then create the requested branch. Only after the human has
approved the package plan, use Codex or Claude Code to distribute the
approved packages across their owning modules and work on independent
packages concurrently without cross-module pollution. This AGENT_CALL
requires the experimental agent-steps setting to be enabled and an
authenticated Codex or Claude Code account selected; abort with that
requirement if it is not.

Integrate the results, apply the `principal-developer` skill
(`skills/principal-developer/SKILL.md`) as a review gate, then run `cargo fmt
--check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo
test --all-targets --all-features`, and all ignored `live_spec_planning`
tests, fixing findings and repeating relevant checks until clean. Confirm the
diff carries no credentials, personal data, proprietary third-party code, or
license-incompatible generated material. Propose a pull-request title and
body describing the change, why it is needed, how it was tested, and any
follow-up work to the human, and only if they approve, push and create the
pull request with `gh` CLI.
