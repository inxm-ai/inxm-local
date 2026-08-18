---
name: Support report
about: >-
  Report a problem with a plan, run, or the inxm-local app itself. Mirrors the
  report produced by the in-app "Create support ticket" button.
title: "[support] "
labels: ["support"]
---

<!--
  Tip: the in-app "Create support ticket" button (Plan-Chat panel, run
  failure card, or MCP editor) generates this same report automatically —
  version, host, plan, and run timeline included, with all input/output
  values anonymized and credentials masked — and opens it here prefilled.
  Use that first if you can; fill in this template by hand otherwise.
-->

## Support report

| | |
|---|---|
| inxm-local | v |
| Host | <!-- OS, interpreters, runners, e.g. "Linux, python3.11, node20" --> |
| Compiler backend | <!-- e.g. "claude (sonnet)", or "not configured" --> |

## Plan "<name>"

- id: `<plan-id>` (v<version>)
- compiled by: <!-- optional -->
- intent: <!-- optional -->

| step | type | depends on |
|---|---|---|
| `<step-id>` | <!-- step type --> | <!-- `dep-id` or — --> |

<details><summary>Full plan definition (scrubbed)</summary>

```yaml
# Paste the plan YAML here. Remove or mask any credentials, tokens, emails,
# or local file paths before submitting.
```

</details>

## Run `<run-id>`

- status: <!-- succeeded / failed / ... -->
- started: <!-- timestamp -->
- finished: <!-- timestamp -->
- invocation inputs (anonymized): `<!-- e.g. {"key": "[string, 12 chars]"} -->`

| step | status | attempt | duration | outputs (shape) |
|---|---|---|---|---|
| `<step-id>` | <!-- status --> | <!-- attempt # --> | <!-- N ms --> | <!-- e.g. result: string (42 chars) --> |

**Error in `<step-id>`:**

```
<!-- scrubbed error text -->
```

---
_Please anonymize input/output values and mask credentials before submitting._
