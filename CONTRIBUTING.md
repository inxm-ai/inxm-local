# Contributing to INXM Local

Thank you for contributing to INXM Local. Please keep changes focused, explain
the motivation in the pull request, and add or update tests where behavior
changes.

## Development checks

Before opening a pull request, run:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Do not include credentials, personal data, proprietary third-party code, or
generated material whose license is incompatible with Apache-2.0. Contributors
remain responsible for reviewing and testing AI-assisted work before submitting
it.

## Agent-assisted contributions

`examples/dogfooding/` contains reusable, MCP-compiled plans (prompt-to-plan,
feature-development, bugfix) that drive Codex or Claude Code through the
`principal-developer` skill and these same development checks end to end,
stopping for human approval before `gh pr create`. Using one of these plans is
an accepted way to open a pull request here — see
`examples/dogfooding/README.md` for details. It does not change any of the
requirements below.

## Pull requests

Describe the change, why it is needed, how it was tested, and any follow-up work.
Keep unrelated changes in separate pull requests. By opening a pull request,
you confirm that you have the right to submit every part of the contribution.

## License

INXM Local is licensed under the Apache License, Version 2.0. By contributing,
you agree that your contribution is licensed under the same license and make
the contributor affirmations below.

## Contributor License Agreement (CLA)

By contributing to this project, you affirm that:

1. You have the right to submit the contribution.
2. Your contribution does not violate any third-party rights.
3. You submit your contribution under the Apache License, Version 2.0,
   including its copyright and patent license terms.

If you contribute on behalf of an organization, you confirm that you have the
authority to make the contribution and these grants on its behalf.

The CLA Assistant will ask first-time contributors to post the following exact
text on their pull request:

> I have read the CLA Document and I hereby sign the CLA

The signature is recorded by GitHub account in the `inxm-ai/cla-signatures`
repository. Contributors normally need to sign only once for this project.
