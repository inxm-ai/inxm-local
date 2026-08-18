//! Chat input parser — slash commands and free-form intent.
//!
//! Plain text is treated as compile intent ("chat to create a plan"); lines
//! starting with `/` are commands. Pure functions, no UI or I/O.

// ─── Command table ────────────────────────────────────────────────────────────

/// Static metadata for a slash command — drives the input palette and /help.
pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/compile",
        usage: "/compile <intent…>",
        description: "Compile natural language into a new plan (new chats only)",
    },
    CommandSpec {
        name: "/plans",
        usage: "/plans",
        description: "List stored plans",
    },
    CommandSpec {
        name: "/show",
        usage: "/show <plan>",
        description: "Display a plan (id prefix or name)",
    },
    CommandSpec {
        name: "/edit",
        usage: "/edit <change…>",
        description: "Propose an LLM-assisted edit to the plan attached to this chat, for review",
    },
    CommandSpec {
        name: "/run",
        usage: "/run [--inputs '<json>']",
        description: "Execute the plan attached to this chat with optional invocation inputs",
    },
    CommandSpec {
        name: "/runs",
        usage: "/runs",
        description: "List recent runs",
    },
    CommandSpec {
        name: "/inspect",
        usage: "/inspect [run-id]",
        description: "Inspect this plan's latest run (or a specified run)",
    },
    CommandSpec {
        name: "/repair",
        usage: "/repair [run-id]",
        description: "Repair the latest failed run in this chat (or a specified run)",
    },
    CommandSpec {
        name: "/resume",
        usage: "/resume <run-id> [--inputs '<json>']",
        description: "Re-run a failed run's failing step and everything downstream of it",
    },
    CommandSpec {
        name: "/apply",
        usage: "/apply <patch-id>",
        description: "Approve and apply a proposed patch or plan edit",
    },
    CommandSpec {
        name: "/reject",
        usage: "/reject <patch-id> [reason…]",
        description: "Reject a proposed patch or plan edit",
    },
    CommandSpec {
        name: "/schedule",
        usage: "/schedule <plan> <cron> [--inputs '<json>']",
        description: "Schedule a plan with captured invocation inputs (local time)",
    },
    CommandSpec {
        name: "/schedules",
        usage: "/schedules",
        description: "List schedules (manage under Plans)",
    },
    CommandSpec {
        name: "/tools",
        usage: "/tools",
        description: "List the tool catalog (manage under MCP Tools)",
    },
    CommandSpec {
        name: "/support",
        usage: "/support [run-id]",
        description: "Create a support ticket — collects an anonymized report and opens a prefilled GitHub issue",
    },
    CommandSpec {
        name: "/help",
        usage: "/help",
        description: "Show all commands",
    },
    CommandSpec {
        name: "/clear",
        usage: "/clear",
        description: "Clear the conversation",
    },
];

/// Commands whose first argument is a plan reference (used for completion).
pub fn wants_plan_arg(command: &str) -> bool {
    matches!(command, "/show" | "/schedule")
}

/// Commands whose first argument is a run id (used for completion).
pub fn wants_run_arg(command: &str) -> bool {
    matches!(command, "/inspect" | "/repair" | "/resume" | "/support")
}

/// Commands whose first argument is a patch id (used for completion).
pub fn wants_patch_arg(command: &str) -> bool {
    matches!(command, "/apply" | "/reject")
}

// ─── Parsed input ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ChatInput {
    /// Free-form text — compile it into a plan.
    Intent(String),
    Command(Command),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Compile {
        intent: String,
    },
    Plans,
    Show {
        plan_ref: String,
    },
    Edit {
        instruction: String,
    },
    Run {
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    Runs,
    Inspect {
        run_id: Option<String>,
    },
    Repair {
        run_id: Option<String>,
    },
    /// Re-run a failed run's failing step (and everything downstream of it)
    /// against the plan's current version — normally issued right after a
    /// repair patch has been applied.
    Resume {
        run_id: String,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    Apply {
        patch_id: String,
    },
    Reject {
        patch_id: String,
        reason: Option<String>,
    },
    Schedule {
        plan_ref: String,
        cron: String,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    Schedules,
    Tools,
    /// Collect an anonymized support report (plan + latest failed run) and
    /// open a prefilled GitHub issue.
    Support {
        run_id: Option<String>,
    },
    Help,
    Clear,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    #[error("unknown command '{0}' — type /help to list commands")]
    UnknownCommand(String),
    #[error("missing argument — usage: {usage}")]
    MissingArg { usage: &'static str },
    #[error("unmatched quotes in input")]
    UnmatchedQuotes,
    #[error("invalid --inputs JSON object: {0}")]
    InvalidInputs(String),
    #[error("unexpected argument — usage: {usage}")]
    UnexpectedArg { usage: &'static str },
}

/// Parse one line of chat input.
pub fn parse(input: &str) -> Result<ChatInput, ParseError> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return Ok(ChatInput::Intent(trimmed.to_owned()));
    }

    let command_name = trimmed.split_whitespace().next().unwrap_or(trimmed);
    match command_name {
        "/compile" => {
            return Ok(ChatInput::Command(Command::Compile {
                intent: require_raw_tail(trimmed, command_name, "/compile <intent…>")?,
            }));
        }
        "/edit" => {
            return Ok(ChatInput::Command(Command::Edit {
                instruction: require_raw_tail(trimmed, command_name, "/edit <change…>")?,
            }));
        }
        _ => {}
    }

    let tokens = shlex::split(trimmed).ok_or(ParseError::UnmatchedQuotes)?;
    let (head, rest) = tokens
        .split_first()
        .expect("slash input always yields at least one token");

    let command = match head.as_str() {
        "/plans" => Command::Plans,
        "/show" => Command::Show {
            plan_ref: require_one(rest, "/show <plan>")?,
        },
        "/run" => {
            let (remaining, inputs) = take_inputs(rest)?;
            if !remaining.is_empty() {
                return Err(ParseError::UnexpectedArg {
                    usage: "/run [--inputs '<json>']",
                });
            }
            Command::Run { inputs }
        }
        "/runs" => Command::Runs,
        "/inspect" => Command::Inspect {
            run_id: rest.first().cloned(),
        },
        "/repair" => Command::Repair {
            run_id: rest.first().cloned(),
        },
        "/resume" => {
            let (remaining, inputs) = take_inputs(rest)?;
            if remaining.is_empty() {
                return Err(ParseError::MissingArg {
                    usage: "/resume <run-id> [--inputs '<json>']",
                });
            }
            if remaining.len() != 1 {
                return Err(ParseError::UnexpectedArg {
                    usage: "/resume <run-id> [--inputs '<json>']",
                });
            }
            Command::Resume {
                run_id: require_one(&remaining, "/resume <run-id> [--inputs '<json>']")?,
                inputs,
            }
        }
        "/apply" => Command::Apply {
            patch_id: require_one(rest, "/apply <patch-id>")?,
        },
        "/reject" => {
            let patch_id = require_one(rest, "/reject <patch-id> [reason…]")?;
            let reason = (rest.len() > 1).then(|| rest[1..].join(" "));
            Command::Reject { patch_id, reason }
        }
        "/schedule" => {
            let (remaining, inputs) = take_inputs(rest)?;
            let plan_ref = require_one(&remaining, "/schedule <plan> <cron> [--inputs '<json>']")?;
            let cron = remaining[1..].join(" ");
            if cron.trim().is_empty() {
                return Err(ParseError::MissingArg {
                    usage: "/schedule <plan> <cron> [--inputs '<json>']",
                });
            }
            Command::Schedule {
                plan_ref,
                cron,
                inputs,
            }
        }
        "/schedules" => Command::Schedules,
        "/support" => Command::Support {
            run_id: rest.first().cloned(),
        },
        "/tools" => Command::Tools,
        "/help" => Command::Help,
        "/clear" => Command::Clear,
        other => return Err(ParseError::UnknownCommand(other.to_owned())),
    };

    Ok(ChatInput::Command(command))
}

/// Command-name completions for a partially typed first token.
pub fn completions(prefix: &str) -> Vec<&'static CommandSpec> {
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        .collect()
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn take_inputs(
    tokens: &[String],
) -> Result<(Vec<String>, indexmap::IndexMap<String, serde_json::Value>), ParseError> {
    let mut remaining = Vec::new();
    let mut inputs = indexmap::IndexMap::new();
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] == "--inputs" {
            let raw = tokens.get(index + 1).ok_or_else(|| {
                ParseError::InvalidInputs("--inputs requires a JSON object".to_owned())
            })?;
            inputs = serde_json::from_str(raw)
                .map_err(|error| ParseError::InvalidInputs(error.to_string()))?;
            index += 2;
        } else {
            remaining.push(tokens[index].clone());
            index += 1;
        }
    }
    Ok((remaining, inputs))
}

fn require_one(rest: &[String], usage: &'static str) -> Result<String, ParseError> {
    rest.first()
        .cloned()
        .ok_or(ParseError::MissingArg { usage })
}

fn require_raw_tail(
    input: &str,
    command_name: &str,
    usage: &'static str,
) -> Result<String, ParseError> {
    let tail = input[command_name.len()..].trim();
    if tail.is_empty() {
        return Err(ParseError::MissingArg { usage });
    }
    Ok(tail.to_owned())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_intent() {
        assert_eq!(
            parse("fetch the bitcoin price"),
            Ok(ChatInput::Intent("fetch the bitcoin price".to_owned()))
        );
    }

    #[test]
    fn compile_joins_all_words() {
        assert_eq!(
            parse("/compile fetch the bitcoin price"),
            Ok(ChatInput::Command(Command::Compile {
                intent: "fetch the bitcoin price".to_owned()
            }))
        );
    }

    #[test]
    fn compile_respects_quotes() {
        assert_eq!(
            parse(r#"/compile "a quoted intent""#),
            Ok(ChatInput::Command(Command::Compile {
                intent: r#""a quoted intent""#.to_owned()
            }))
        );
    }

    #[test]
    fn edit_uses_the_whole_tail_as_the_instruction() {
        assert_eq!(
            parse("/edit add an approval step before writing"),
            Ok(ChatInput::Command(Command::Edit {
                instruction: "add an approval step before writing".to_owned(),
            }))
        );
        assert_eq!(
            parse(r#"/edit "save output to a JSON file""#),
            Ok(ChatInput::Command(Command::Edit {
                instruction: r#""save output to a JSON file""#.to_owned(),
            }))
        );
    }

    #[test]
    fn edit_preserves_unmatched_quotes_in_the_instruction() {
        let instruction =
            r#"Multiple problems: `git commit -m "$MSG"` fails with 'nothing to commit"#;

        assert_eq!(
            parse(&format!("/edit {instruction}")),
            Ok(ChatInput::Command(Command::Edit {
                instruction: instruction.to_owned(),
            }))
        );
    }

    #[test]
    fn compile_preserves_quotes_in_the_intent() {
        let intent = r#"explain why `git commit -m "$MSG"` fails"#;

        assert_eq!(
            parse(&format!("/compile {intent}")),
            Ok(ChatInput::Command(Command::Compile {
                intent: intent.to_owned(),
            }))
        );
    }

    #[test]
    fn run_parses_typed_inputs_json() {
        let parsed = parse(r#"/run --inputs '{"query":"rust","limit":5,"fresh":true}'"#).unwrap();
        let ChatInput::Command(Command::Run { inputs }) = parsed else {
            panic!("expected run command")
        };
        assert_eq!(inputs["query"], serde_json::json!("rust"));
        assert_eq!(inputs["limit"], serde_json::json!(5));
        assert_eq!(inputs["fresh"], serde_json::json!(true));
    }

    #[test]
    fn run_accepts_no_arguments() {
        assert_eq!(
            parse("/run"),
            Ok(ChatInput::Command(Command::Run {
                inputs: Default::default(),
            }))
        );
        assert_eq!(
            parse("/run another-plan"),
            Err(ParseError::UnexpectedArg {
                usage: "/run [--inputs '<json>']"
            })
        );
    }

    #[test]
    fn missing_args_are_reported_with_usage() {
        assert_eq!(
            parse("/compile"),
            Err(ParseError::MissingArg {
                usage: "/compile <intent…>"
            })
        );
        assert_eq!(
            parse("/edit"),
            Err(ParseError::MissingArg {
                usage: "/edit <change…>"
            })
        );
    }

    #[test]
    fn repair_defaults_to_the_chat_context_and_accepts_an_override() {
        assert_eq!(
            parse("/repair"),
            Ok(ChatInput::Command(Command::Repair { run_id: None }))
        );
        assert_eq!(
            parse("/repair abc123"),
            Ok(ChatInput::Command(Command::Repair {
                run_id: Some("abc123".to_owned()),
            }))
        );
    }

    #[test]
    fn inspect_defaults_to_the_chat_context_and_accepts_an_override() {
        assert_eq!(
            parse("/inspect"),
            Ok(ChatInput::Command(Command::Inspect { run_id: None }))
        );
        assert_eq!(
            parse("/inspect abc123"),
            Ok(ChatInput::Command(Command::Inspect {
                run_id: Some("abc123".to_owned()),
            }))
        );
    }

    #[test]
    fn resume_takes_a_run_id() {
        assert_eq!(
            parse("/resume abc123"),
            Ok(ChatInput::Command(Command::Resume {
                run_id: "abc123".to_owned(),
                inputs: Default::default(),
            }))
        );
        assert_eq!(
            parse("/resume"),
            Err(ParseError::MissingArg {
                usage: "/resume <run-id> [--inputs '<json>']"
            })
        );
    }

    #[test]
    fn resume_parses_typed_input_overrides() {
        let parsed = parse(r#"/resume abc123 --inputs '{"output_path":"fixed.txt"}'"#).unwrap();
        assert_eq!(
            parsed,
            ChatInput::Command(Command::Resume {
                run_id: "abc123".to_owned(),
                inputs: indexmap::indexmap! {
                    "output_path".to_owned() => serde_json::json!("fixed.txt"),
                },
            })
        );
    }

    #[test]
    fn reject_collects_reason() {
        assert_eq!(
            parse("/reject p1 not the right fix"),
            Ok(ChatInput::Command(Command::Reject {
                patch_id: "p1".to_owned(),
                reason: Some("not the right fix".to_owned()),
            }))
        );
    }

    #[test]
    fn schedule_takes_plan_and_cron_tail() {
        assert_eq!(
            parse("/schedule my-plan 0 8 * * 1"),
            Ok(ChatInput::Command(Command::Schedule {
                plan_ref: "my-plan".to_owned(),
                cron: "0 8 * * 1".to_owned(),
                inputs: Default::default(),
            }))
        );
        assert_eq!(
            parse("/schedule my-plan"),
            Err(ParseError::MissingArg {
                usage: "/schedule <plan> <cron> [--inputs '<json>']"
            })
        );
    }

    #[test]
    fn support_defaults_to_the_chat_context_and_accepts_an_override() {
        assert_eq!(
            parse("/support"),
            Ok(ChatInput::Command(Command::Support { run_id: None }))
        );
        assert_eq!(
            parse("/support abc123"),
            Ok(ChatInput::Command(Command::Support {
                run_id: Some("abc123".to_owned()),
            }))
        );
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert_eq!(
            parse("/frobnicate"),
            Err(ParseError::UnknownCommand("/frobnicate".to_owned()))
        );
    }

    #[test]
    fn unmatched_quotes_are_an_error() {
        assert_eq!(
            parse(r#"/run --inputs '{"query":"oops"}"#),
            Err(ParseError::UnmatchedQuotes)
        );
    }

    #[test]
    fn completions_filter_by_prefix() {
        let all = completions("/");
        assert_eq!(all.len(), COMMANDS.len());
        let r: Vec<_> = completions("/re").iter().map(|c| c.name).collect();
        assert_eq!(r, vec!["/repair", "/resume", "/reject"]);
        let e: Vec<_> = completions("/ed").iter().map(|c| c.name).collect();
        assert_eq!(e, vec!["/edit"]);
    }

    #[test]
    fn every_command_in_table_parses_or_reports_usage() {
        for spec in COMMANDS {
            match parse(spec.name) {
                Ok(_)
                | Err(ParseError::MissingArg { .. })
                | Err(ParseError::UnexpectedArg { .. }) => {}
                other => panic!("{} produced unexpected result: {other:?}", spec.name),
            }
        }
    }
}
