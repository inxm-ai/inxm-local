# Test and validate changes

Run the same checks used by continuous integration before opening a pull request.

## Standard checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The CI workflow runs these checks on pull requests and pushes to `main`. Linux CI installs the native GTK, AppIndicator, OpenSSL, XCB, and XKB packages before building.

## MCP self-test

The repository includes a live HTTP flow that starts the local MCP server, lists tools, executes the seeded `echo` tool, inspects persisted inputs, and exercises schedule storage:

```sh
INXM_MCP_SELF_TEST=1 INXM_LOCAL_DATA_DIR=target/mcp-self-test cargo run
```

Successful output includes `MCP self-test passed`.

## Live specification tests

The live planning specification test is ignored by default and requires its documented external setup. Inspect the test file before running it:

```sh
cargo test --test live_spec_planning -- --ignored
```

## Test isolation

Set `INXM_LOCAL_DATA_DIR` to a temporary or `target/` directory when running an app or MCP process manually. This prevents local plans, schedules, credentials references, and telemetry counters from mixing with a normal installation.

## Native UI screenshots

The desktop screenshot hook captures the viewport after the app has rendered. Build the binary first, then run it with an isolated data directory and a view to inspect:

```sh
cargo build --bin inxm-local
INXM_LOCAL_DATA_DIR="$(mktemp -d)" INXM_VIEW=mcp \
	INXM_SCREENSHOT="$HOME/Desktop/inxm-mcp-review.png" target/debug/inxm-local
```

`INXM_VIEW` accepts `chat`, `plans`, `runs`, `schedules`, `mcp`, or `settings`. Set `INXM_DEMO=2` to populate the chat workspace without using a compiler key. Wait for `screenshot saved: ...`, then stop the process with Ctrl+C: the window closes after capture, but the tray process may remain running. A screenshot verifies only the rendered state; exercise changed controls in the running app separately and report interactions that could not be verified.

<h2>What's next</h2>

- [Set up the repository](setup.md) if you still need a working development environment.
- [Exercise the Local MCP server](../reference/mcp-server.md) to verify client integrations.
- [Prepare a contribution](contributing.md) once your checks are passing.