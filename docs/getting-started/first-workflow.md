# Create your first workflow

This tutorial creates a plan with a typed input, runs it, and inspects the result.

---

## Before you begin

Install INXM Local and configure a compiler connection under **Settings -> Compiler**. INXM Local needs access to a large language model to turn your description into a workflow plan. You can connect an API-based backend, use a supported account-backed CLI such as Claude Code or Codex, or configure a compatible custom endpoint. If you already pay for access through one of those supported accounts, you can use that login instead of configuring a separate API key.

---

## Compile a plan

1. Open a new chat.
2. Describe a small workflow with a value that should change between runs. For example: `Echo the supplied message.`
3. Review the generated design and approve it.
4. Confirm that the compiled plan declares a `message` input.

You can also start a new chat explicitly with `/compile Echo the supplied message`. The `/compile` command is available only in a new empty chat. Plain text in a new chat is treated as compile intent.

---

## Run the plan

Run it from the plan chat with an input value:

```text
/run --inputs '{"message":"Hello from INXM Local"}'
```

The executor validates required and unknown inputs before starting. The plan card displays step progress and the run result.

---

## Inspect the result

Use `/inspect` to see the latest run, including step status, timing, outputs, and errors. Use `/runs` to list recent runs.

**Congratulations!** 🥳

*You have built, run, and inspected your first INXM Local plan. From here, you can reuse it with different inputs, connect it to tools, or explore how plans, runs, and repairs fit together.*

---

## What's next

- Understand [plans, runs, and repairs](../concepts/plans-and-runs.md).
- Learn all [chat workflow commands](../user-guide/chat-workflows.md).
- Add a [tool or MCP server](../user-guide/tools.md).