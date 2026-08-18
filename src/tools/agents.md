# tools module

Tool execution entry point. Dispatches to an adapter by `ToolConfig` variant.

Files:
- `catalog.rs` — tool catalog and `ToolConfig`
- `adapters/` — per-tool-type execution (subprocess, HTTP, MCP, ...)

You own this dir only. Read other `src/` dirs for context, never edit them.
