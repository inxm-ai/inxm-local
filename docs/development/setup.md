# Set up the repository

Use this path to build and run INXM Local from source.

## Prerequisites

- Install a stable Rust toolchain with Cargo.
- Linux builds also require the native packages used by the CI workflow:
	- GTK 3
	- Ayatana AppIndicator
	- OpenSSL
	- XCB/XKB development libraries

## Build and run

From the repository root:

```sh
cargo run --release
```

The debug build is useful for iteration:

```sh
cargo run
```

Use `INXM_LOCAL_DATA_DIR` to isolate local data from an existing installation:

```sh
INXM_LOCAL_DATA_DIR=target/dev-data cargo run
```

## Run without a window

Start the local MCP server and scheduler with:

```sh
cargo run -- --headless
```

Start only the MCP endpoint with:

```sh
INXM_MCP_ONLY=1 cargo run
```

## Repository map

| Area | Responsibility |
| --- | --- |
| `src/app` | Desktop UI, engine bridge, scheduler, local MCP server |
| `src/compiler` | LLM-backed plan compilation |
| `src/validator` | Plan and tool contract validation |
| `src/executor` | Dependency-aware step execution |
| `src/repair` | Failure classification and repair patches |
| `src/plan` | Plan data types and normalization |
| `src/storage` | Persistent plans, runs, schedules, and patches |
| `src/tools` | Tool catalog and subprocess, HTTP, and MCP adapters |
| `tests` | Integration and live specification tests |

<h2>What's next</h2>

- [Explore the system architecture](architecture.md) to see how the application pieces fit together.
- [Run the test and validation workflow](testing.md) before changing the code.
- [Read the contribution guide](contributing.md) to prepare a focused change.