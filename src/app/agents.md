# app module

Desktop UI layer (egui). Chat-first surface over the compiler/executor core.

Files:
- `theme.rs` — colors/spacing tokens, only source of style
- `anim.rs` — entrance/pulse animation helpers
- `widgets.rs` — small UI atoms (badges, dots, buttons)
- `views/` — chat, plan cards, plans list, MCP manager, schedules, settings
- `engine.rs` — async bridge to compiler/executor
- `chat_store.rs`, `schedule_store.rs` — chat/schedule persistence
- `mcp_server.rs` — local MCP server
- `commands.rs` — slash commands
- `mod.rs` — shell: nav, event routing, layout

You own this dir only. Read other `src/` dirs for context, never edit them.
