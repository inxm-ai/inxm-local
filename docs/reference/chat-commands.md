# Chat command reference

Enter commands in the chat input. Plain text starts a guided plan-creation flow in a new, empty chat, continues an active flow, or asks an insight question in an existing conversation.

| Command | Behavior |
| --- | --- |
| `/compile <intent>` | Compile a new plan |
| `/plans` | List stored plans |
| `/show <plan>` | Open a plan by name or ID prefix |
| `/edit <change>` | Propose an edit to the plan attached to the chat |
| `/run [--inputs '<json>']` | Execute the attached plan |
| `/runs` | List recent runs |
| `/inspect [run-id]` | Inspect the latest or specified run |
| `/repair [run-id]` | Propose a repair for a failed run |
| `/resume <run-id> [--inputs '<json>']` | Rerun a failed step and downstream work |
| `/apply <patch-id>` | Apply a proposed patch or plan edit |
| `/reject <patch-id> [reason]` | Reject a proposed patch or plan edit |
| `/schedule <plan> <cron> [--inputs '<json>']` | Create a local-time schedule |
| `/schedules` | List schedules |
| `/tools` | List the tool catalog |
| `/support [run-id]` | Create an anonymized support report and open a prefilled GitHub issue |
| `/help` | Show command help |
| `/clear` | Clear the conversation |

The `--inputs` value must be a JSON object. Plan inputs are validated before runs and schedules are created.