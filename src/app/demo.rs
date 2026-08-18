//! Dev-only demo seeding for screenshots and design iteration.
//!
//! `INXM_DEMO=1` pre-fills the chat with a realistic conversation (plan card
//! mid-run, summaries, an error) without needing a compiler key.
//! `INXM_VIEW=chat|plans|mcp|settings` picks the initial view.
//! Both are inert unless set.

use indexmap::IndexMap;

use crate::executor::StepRunStatus;
use crate::plan::types::{
    CodeCallConfig, HumanInteractionConfig, Plan, PlanMetadata, PlanStep, PromptCallConfig,
    StepConfig, ToolCallConfig,
};

use super::View;
use super::views::chat::{ChatState, MessageBody, Role};
use super::views::plan_card::RunBinding;

const DEMO_ENV: &str = "INXM_DEMO";
const VIEW_ENV: &str = "INXM_VIEW";

/// Explicit `INXM_VIEW` override, if set to a recognized value. `None` means
/// no override was requested — the caller (`InxmApp::new`) picks the real
/// default, e.g. based on whether the user has any compiled plans yet.
pub fn initial_view_override() -> Option<View> {
    match std::env::var(VIEW_ENV).ok()?.as_str() {
        "chat" => Some(View::Chat),
        "plans" => Some(View::Plans),
        "runs" => Some(View::Runs),
        "schedules" => Some(View::Schedules),
        "mcp" => Some(View::Mcp),
        "settings" => Some(View::Settings),
        _ => None,
    }
}

pub fn initial_chat() -> ChatState {
    match std::env::var(DEMO_ENV).is_ok_and(|v| v == "1" || v == "2" || v == "3") {
        true => demo_chat(),
        false => ChatState::default(),
    }
}

/// `INXM_DEMO=3` starts with both collapsible panels collapsed, for
/// screenshots of the reclaimed-space layout.
pub fn initial_panels_collapsed(ctx: &egui::Context) -> bool {
    let collapsed = std::env::var(DEMO_ENV).is_ok_and(|v| v == "3");
    if collapsed {
        ctx.data_mut(|data| {
            data.insert_temp(egui::Id::new("chat_right_panel_collapsed"), true);
        });
    }
    collapsed
}

/// In demo mode, pre-expand one step's detail panel so screenshots exercise
/// the inspection view without interaction. `INXM_DEMO=2` keeps the demo
/// transcript but skips this takeover so the plain chat + workspace split
/// stays visible for layout screenshots.
pub fn preopen_demo_detail(ctx: &egui::Context) {
    if !std::env::var(DEMO_ENV).is_ok_and(|v| v == "1") {
        return;
    }
    let workspace_id = egui::Id::new("owned_plan_workspace").with("demo");
    let detail_id = workspace_id
        .with("expanded")
        .with("graph")
        .with("step")
        .with("summarize")
        .with("detail_open");
    ctx.data_mut(|d| {
        d.insert_temp(detail_id, true);
        d.insert_temp(
            workspace_id.with("workspace_expanded"),
            "demo-run".to_owned(),
        );
    });
}

fn demo_chat() -> ChatState {
    let mut chat = ChatState::default();
    // Opens the command palette on launch so screenshots cover its layout.
    chat.input = "/run".to_owned();
    chat.push(
        Role::User,
        MessageBody::Text(
            "fetch the top hacker news stories, summarize them, and ask me before saving"
                .to_owned(),
        ),
    );
    chat.push(
        Role::Assistant,
        MessageBody::Text(
            "Compiled “hn-digest” — 4 steps, validated and saved. Run it when ready.".to_owned(),
        ),
    );
    let mut plan = demo_plan();
    plan.metadata.id = "demo".to_owned();
    chat.attach_plan(Box::new(plan));
    chat.workspace_run = Some(demo_binding());
    chat.push(
        Role::Assistant,
        MessageBody::Error(
            "Run failed at step “save-digest” after 4.2 s: subprocess exited with status 1 \
             (No such file or directory). Use /repair to ask the compiler for a fix."
                .to_owned(),
        ),
    );
    chat.push(
        Role::Assistant,
        MessageBody::SupportTicket {
            issue_url: "https://github.com/inxm-ai/inxm-local/issues/new".to_owned(),
            report_path: "/Users/demo/Library/Application Support/ai.inxm.inxm-local/\
                          support-tickets/support-20260804-153658.md"
                .to_owned(),
            text: "Support report collected — plan structure and run timeline included, all \
                   input/output values anonymized and credentials masked. Saved to \
                   `/Users/demo/Library/Application Support/ai.inxm.inxm-local/support-tickets/\
                   support-20260804-153658.md` for review."
                .to_owned(),
        },
    );
    chat.push(
        Role::Assistant,
        MessageBody::RunCompleted {
            run_id: "demo-run".to_owned(),
            text: "Run succeeded in 8.6 s.\nTokens: 1 AI step — 10 in / 122 out (132 total).\n\n\
                   Final result:\n- weather_summary: Munich is seeing mainly clear skies with a \
                   hot temperature of 35.3°C and a light 10.1 km/h wind blowing from the \
                   north-northeast."
                .to_owned(),
        },
    );
    chat
}

fn step(id: &str, name: &str, config: StepConfig, deps: &[&str]) -> PlanStep {
    PlanStep {
        id: id.to_owned(),
        name: name.to_owned(),
        description: Some(format!("Demo step “{name}”")),
        config,
        depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    }
}

fn demo_plan() -> Plan {
    let fetch = step(
        "fetch-stories",
        "Fetch top stories",
        StepConfig::ToolCall(ToolCallConfig {
            tool: "http-get".to_owned(),
            arguments: IndexMap::new(),
        }),
        &[],
    );
    let summarize = step(
        "summarize",
        "Summarize with one prompt call",
        StepConfig::PromptCall(PromptCallConfig {
            model: "claude-sonnet-4-6".to_owned(),
            system_prompt: None,
            user_prompt: "Summarize: ${step.fetch-stories.body}".to_owned(),
            output_field: "summary".to_owned(),
            max_tokens: None,
            temperature: None,
        }),
        &["fetch-stories"],
    );
    let approve = step(
        "confirm",
        "Ask before saving",
        StepConfig::HumanInteraction(HumanInteractionConfig {
            prompt: "Save this digest to disk?".to_owned(),
            response_field: "answer".to_owned(),
            approval_required: true,
        }),
        &["summarize"],
    );
    let save = step(
        "save-digest",
        "Write digest to disk",
        StepConfig::CodeCall(CodeCallConfig {
            language: "bash".to_owned(),
            inline: Some("cat > digest.md".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: IndexMap::new(),
            working_dir: None,
            timeout_secs: None,
        }),
        &["confirm"],
    );

    Plan {
        metadata: PlanMetadata::new(Some(
            "fetch the top hacker news stories, summarize them, and ask me before saving"
                .to_owned(),
        )),
        name: "hn-digest".to_owned(),
        description: None,
        inputs: vec![],
        config: IndexMap::new(),
        steps: vec![fetch, summarize, approve, save],
        outputs: vec![],
    }
}

fn demo_binding() -> RunBinding {
    let statuses = [
        ("fetch-stories", StepRunStatus::Succeeded),
        ("summarize", StepRunStatus::Succeeded),
        ("confirm", StepRunStatus::Running),
        ("save-digest", StepRunStatus::Pending),
    ];
    let durations = [("fetch-stories", 412u64), ("summarize", 1856)];
    let stdouts = [(
        "summarize",
        "Top stories today: a new Rust release, a deep dive into local-first \
         software, and a debate about deterministic AI runtimes.",
    )];
    let outputs = [(
        "fetch-stories",
        [("body", serde_json::json!({ "stories": 30, "source": "hn" }))],
    )];
    RunBinding {
        run_id: "demo-run".to_owned(),
        statuses: statuses
            .into_iter()
            .map(|(id, s)| (id.to_owned(), s))
            .collect(),
        durations_ms: durations
            .into_iter()
            .map(|(id, d)| (id.to_owned(), d))
            .collect(),
        stdouts: stdouts
            .into_iter()
            .map(|(id, s)| (id.to_owned(), s.to_owned()))
            .collect(),
        outputs: outputs
            .into_iter()
            .map(|(id, fields)| {
                (
                    id.to_owned(),
                    fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
                )
            })
            .collect(),
        ..Default::default()
    }
}
