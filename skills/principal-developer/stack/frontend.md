# Frontend — egui desktop UI (`src/app`)

Read this for any frontend task involving views, widgets, layout, theming, or
UI-thread behaviour in `inxm-local`.

---

## Stack

Pure-Rust immediate-mode GUI via [`egui`](https://github.com/emilk/egui) +
[`eframe`](https://github.com/emilk/egui/tree/master/crates/eframe) — no webview, no
HTML/CSS/JS, no separate frontend build step. The desktop app is a **chat-first surface**
over the deterministic compiler/executor/repair core; `app/` never contains business
logic that belongs in those modules (see `architecture.md`).

There is no Atomic Design layer system here — egui is immediate-mode, so there's no
persistent component tree to compose that way. Instead, `src/app/` is organised by
*what draws it* and *what state it needs*, following the existing layout:

```
app/
  theme.rs      ← colors/spacing tokens — the only source of style
  anim.rs       ← entrance/pulse animation helpers
  widgets.rs    ← small reusable UI atoms (badges, dots, buttons)
  views/        ← chat, plan cards, plans list, MCP manager, schedules, settings
  engine.rs     ← async bridge to compiler/executor (UI thread stays sync)
  chat_store.rs, schedule_store.rs ← chat/schedule persistence
  mcp_server.rs ← local MCP server
  commands.rs   ← slash commands
  mod.rs        ← shell: nav, event routing, layout
```

---

## Conventions to follow

**Styling lives in `theme.rs` only.** Don't inline colors, spacing, or font sizes in a
view — add or reuse a token in `theme.rs` so the whole UI stays visually consistent and
themeable from one place.

**Widgets are small and reusable, views are not.** `widgets.rs` holds primitives with no
knowledge of business state (a badge, a status dot, a styled button). A `views/*` file
owns a specific screen/panel and is allowed to know about domain types (`Plan`, `RunState`,
`ChatMessage`) — but it should still delegate rendering of small repeated pieces to
`widgets.rs` rather than duplicating `ui.painter()`/layout code.

**Keep the UI thread synchronous.** egui redraws on the UI thread; anything that talks to
the compiler, executor, filesystem, or network goes through `engine.rs`'s async bridge
and comes back as an event, never a blocking call inside `update()`.

**Business logic stays out of the draw loop.** Command parsing (`commands.rs`), chat
persistence (`chat_store.rs`), and schedule persistence (`schedule_store.rs`) are plain
Rust modules independent of `egui::Context` — this is what makes them unit-testable
without spinning up a UI (see `testing.md`).

**Follow upstream egui idioms** beyond the above: prefer `egui::Ui` composition over
custom painting when a built-in widget does the job, keep `Response` handling explicit
(`if ui.button(...).clicked() { ... }`), and avoid per-frame allocations in hot paths
(the whole UI redraws every frame).

**Visual verification**: `debug_shot.rs` provides a dev-only headless
render-and-screenshot hook (`INXM_SCREENSHOT=/path/out.png`, optionally combined with
`INXM_VIEW=` and `INXM_DEMO=1`) for verifying visual changes without a human at the
keyboard. Use it to check layout/styling changes; it is not a substitute for unit
testing the underlying logic.
