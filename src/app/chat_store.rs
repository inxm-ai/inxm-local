//! Chat session persistence — conversations survive restarts and can be
//! reopened from the sidebar.
//!
//! Sessions are JSON files under `<data_dir>/chats/{id}.json`. Live-only
//! message parts (pending responders, transient listings) are degraded to
//! their durable form on save.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::executor::RunStatus;
use crate::plan::types::Plan;
use crate::storage::patches::Patch;
use crate::storage::plan_edits::PlanEdit;

use super::engine::SuggestedAction;
use super::views::chat::{
    AgentTranscriptLine, ChatMessage, ChatState, FlowPromptKind, GuidedFlow, MessageBody, Role,
};
use super::views::plan_card::RunBinding;

const CHATS_DIR: &str = "chats";
const TITLE_MAX_CHARS: usize = 48;

// ─── Stored form ──────────────────────────────────────────────────────────────

/// A serialisable snapshot of one chat message. Transient listings
/// (`/plans`, `/runs`, `/tools` output) are not stored — they would be stale
/// on reload anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// Mirrors MessageBody; sessions hold at most a few hundred messages.
#[allow(clippy::large_enum_variant)]
enum StoredMessage {
    Text {
        role: Role,
        text: String,
    },
    Error {
        text: String,
    },
    RunStarted {
        run_id: String,
        text: String,
    },
    RunFailed {
        run_id: String,
        text: String,
    },
    RunCompleted {
        run_id: String,
        text: String,
    },
    Plan {
        plan: Box<Plan>,
        run: Option<RunBinding>,
    },
    Patch {
        patch: Box<Patch>,
        resolution: Option<String>,
    },
    Edit {
        edit: Box<PlanEdit>,
        resolution: Option<String>,
    },
    Human {
        prompt: String,
        resolution: Option<String>,
    },
    AgentTranscript {
        run_id: String,
        step_id: String,
        lines: Vec<AgentTranscriptLine>,
    },
    /// Guided-flow elicitation. Unlike `Human` prompts it needs no live
    /// responder channel, so an unresolved prompt stays answerable after a
    /// restart (the flow state it drives is persisted alongside).
    FlowPrompt {
        /// Named `prompt_kind` because `kind` is this enum's serde tag.
        prompt_kind: FlowPromptKind,
        prompt: String,
        resolution: Option<String>,
    },
    SupportTicket {
        issue_url: String,
        report_path: String,
        text: String,
    },
    Help,
    /// An answer to a plain-text question. Unlike `Human` prompts it needs
    /// no live channel, so an unresolved suggested action stays clickable
    /// after a restart.
    Insight {
        answer: String,
        action: Option<SuggestedAction>,
        resolution: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSession {
    id: String,
    title: String,
    /// A conversation owns at most one plan. The plan itself remains in the
    /// versioned plan store; this is the durable relationship used by navigation.
    plan_id: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    messages: Vec<StoredMessage>,
    /// In-progress guided create-a-plan flow, so reopening the app resumes
    /// the refine/design conversation instead of stranding the user.
    /// Transient in-flight flags inside are `#[serde(skip)]` and reset on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flow: Option<GuidedFlow>,
}

/// Sidebar listing entry.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub plan_id: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// The conversation contains an unresolved prompt, design approval, or
    /// patch that needs the user's attention. Kept in the summary so the
    /// sidebar status does not change merely because another conversation is
    /// selected.
    pub awaiting_input: bool,
}

// ─── Conversions ──────────────────────────────────────────────────────────────

fn to_stored(message: &ChatMessage) -> Option<StoredMessage> {
    match &message.body {
        MessageBody::Text(text) => Some(StoredMessage::Text {
            role: message.role,
            text: text.clone(),
        }),
        MessageBody::Error(text) => Some(StoredMessage::Error { text: text.clone() }),
        MessageBody::RunStarted { run_id, text, .. } => Some(StoredMessage::RunStarted {
            run_id: run_id.clone(),
            text: text.clone(),
        }),
        MessageBody::RunFailed { run_id, text, .. } => Some(StoredMessage::RunFailed {
            run_id: run_id.clone(),
            text: text.clone(),
        }),
        MessageBody::RunCompleted { run_id, text } => Some(StoredMessage::RunCompleted {
            run_id: run_id.clone(),
            text: text.clone(),
        }),
        MessageBody::Plan { plan, run } => Some(StoredMessage::Plan {
            plan: plan.clone(),
            run: run.clone(),
        }),
        MessageBody::Patch {
            patch, resolution, ..
        } => Some(StoredMessage::Patch {
            patch: patch.clone(),
            resolution: resolution.clone(),
        }),
        MessageBody::Edit { edit, resolution } => Some(StoredMessage::Edit {
            edit: edit.clone(),
            resolution: resolution.clone(),
        }),
        MessageBody::Human {
            prompt, resolution, ..
        } => Some(StoredMessage::Human {
            prompt: prompt.clone(),
            resolution: resolution.clone(),
        }),
        MessageBody::AgentTranscript {
            run_id,
            step_id,
            lines,
        } => Some(StoredMessage::AgentTranscript {
            run_id: run_id.clone(),
            step_id: step_id.clone(),
            lines: lines.clone(),
        }),
        MessageBody::FlowPrompt {
            kind,
            prompt,
            resolution,
            ..
        } => Some(StoredMessage::FlowPrompt {
            prompt_kind: *kind,
            prompt: prompt.clone(),
            resolution: resolution.clone(),
        }),
        MessageBody::SupportTicket {
            issue_url,
            report_path,
            text,
        } => Some(StoredMessage::SupportTicket {
            issue_url: issue_url.clone(),
            report_path: report_path.clone(),
            text: text.clone(),
        }),
        MessageBody::Help => Some(StoredMessage::Help),
        MessageBody::Insight {
            answer,
            action,
            resolution,
        } => Some(StoredMessage::Insight {
            answer: answer.clone(),
            action: action.clone(),
            resolution: resolution.clone(),
        }),
        MessageBody::PlanIndex(_)
        | MessageBody::RunIndex(_)
        | MessageBody::ToolIndex(_)
        | MessageBody::ScheduleIndex(_) => None,
    }
}

fn to_live(stored: StoredMessage) -> (Role, MessageBody) {
    match stored {
        StoredMessage::Text { role, text } => (role, MessageBody::Text(text)),
        StoredMessage::Error { text } => (Role::Assistant, MessageBody::Error(text)),
        StoredMessage::RunStarted { run_id, text } => (
            Role::Assistant,
            MessageBody::RunStarted {
                run_id,
                text,
                active: false,
            },
        ),
        StoredMessage::RunFailed { run_id, text } => (
            Role::Assistant,
            MessageBody::RunFailed {
                run_id,
                text,
                repair_requested: false,
            },
        ),
        StoredMessage::RunCompleted { run_id, text } => {
            (Role::Assistant, MessageBody::RunCompleted { run_id, text })
        }
        StoredMessage::Plan { plan, run } => {
            // A binding that never finished belongs to a previous process;
            // it cannot still be running.
            let run = run.map(|binding| RunBinding {
                finished: binding.finished.clone().or(Some(RunStatus::Cancelled)),
                ..binding
            });
            (Role::Assistant, MessageBody::Plan { plan, run })
        }
        StoredMessage::Patch { patch, resolution } => (
            Role::Assistant,
            MessageBody::Patch {
                patch,
                resolution,
                resume_requested: false,
            },
        ),
        StoredMessage::Edit { edit, resolution } => {
            (Role::Assistant, MessageBody::Edit { edit, resolution })
        }
        StoredMessage::Human { prompt, resolution } => (
            Role::Assistant,
            MessageBody::Human {
                prompt,
                approval_required: false,
                responder: None,
                draft: String::new(),
                // An unanswered prompt from a dead run can no longer be answered.
                resolution: resolution.or_else(|| Some("(expired)".to_owned())),
            },
        ),
        StoredMessage::AgentTranscript {
            run_id,
            step_id,
            lines,
        } => (
            Role::Assistant,
            MessageBody::AgentTranscript {
                run_id,
                step_id,
                lines,
            },
        ),
        StoredMessage::FlowPrompt {
            prompt_kind,
            prompt,
            resolution,
        } => (
            Role::Assistant,
            // Deliberately kept answerable when unresolved: replying only
            // needs the persisted flow state, not a live channel. If the
            // flow is gone, the chat view expires the prompt on render.
            MessageBody::FlowPrompt {
                kind: prompt_kind,
                prompt,
                resolution,
            },
        ),
        StoredMessage::SupportTicket {
            issue_url,
            report_path,
            text,
        } => (
            Role::Assistant,
            MessageBody::SupportTicket {
                issue_url,
                report_path,
                text,
            },
        ),
        StoredMessage::Help => (Role::Assistant, MessageBody::Help),
        StoredMessage::Insight {
            answer,
            action,
            resolution,
        } => (
            Role::Assistant,
            MessageBody::Insight {
                answer,
                action,
                resolution,
            },
        ),
    }
}

fn session_title(chat: &ChatState) -> String {
    chat.messages
        .iter()
        .find_map(|m| match (&m.role, &m.body) {
            (Role::User, MessageBody::Text(text)) => Some(truncate_chars(text, TITLE_MAX_CHARS)),
            _ => None,
        })
        .or_else(|| chat.plan.as_ref().map(|plan| plan.name.clone()))
        .unwrap_or_else(|| "New Plan-Chat".to_owned())
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.chars().count() <= max {
        true => text.to_owned(),
        false => format!("{}…", text.chars().take(max).collect::<String>()),
    }
}

fn stored_session_awaiting_input(messages: &[StoredMessage], flow: Option<&GuidedFlow>) -> bool {
    flow.is_some_and(GuidedFlow::awaiting_design_approval)
        || messages.iter().rev().any(|message| match message {
            StoredMessage::Human { resolution, .. }
            | StoredMessage::FlowPrompt { resolution, .. } => resolution.is_none(),
            StoredMessage::Patch {
                patch, resolution, ..
            } => {
                resolution.is_none()
                    && patch.status == crate::storage::patches::PatchStatus::Pending
            }
            StoredMessage::Edit { edit, resolution } => {
                resolution.is_none() && edit.status == crate::storage::patches::PatchStatus::Pending
            }
            _ => false,
        })
}

// ─── Store operations ─────────────────────────────────────────────────────────

fn chats_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(CHATS_DIR)
}

fn session_path(data_dir: &Path, id: &str) -> PathBuf {
    chats_dir(data_dir).join(format!("{id}.json"))
}

/// Persist the current conversation. Empty conversations are not written.
pub fn save(
    data_dir: &Path,
    session_id: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    chat: &ChatState,
) -> std::io::Result<()> {
    let messages: Vec<StoredMessage> = chat.messages.iter().filter_map(to_stored).collect();
    if messages.is_empty() && chat.plan.is_none() {
        return Ok(());
    }
    if let Some(plan_id) = chat.plan_id()
        && let Some(owner) = find_by_plan(data_dir, plan_id)
        && owner.id != session_id
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("plan '{plan_id}' already owns chat '{}'", owner.id),
        ));
    }
    let session = StoredSession {
        id: session_id.to_owned(),
        title: session_title(chat),
        plan_id: chat.plan_id().map(str::to_owned),
        created_at,
        updated_at: chrono::Utc::now(),
        messages,
        flow: chat.flow.clone(),
    };
    std::fs::create_dir_all(chats_dir(data_dir))?;
    let json = serde_json::to_string(&session)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(session_path(data_dir, session_id), json)
}

/// Append one message to a session and persist it, preserving the session's
/// plan ownership and creation time. Used by the headless scheduler to record
/// scheduled-run messages the same way the desktop UI does. A missing session
/// is created fresh.
pub fn append(
    data_dir: &Path,
    session_id: &str,
    role: Role,
    body: MessageBody,
) -> std::io::Result<()> {
    let (mut chat, created_at, plan_id) =
        match crate::support::Presence::from_io_result(load(data_dir, session_id)) {
            crate::support::Presence::Found(loaded) => loaded,
            crate::support::Presence::Absent => (ChatState::default(), chrono::Utc::now(), None),
            // The session file exists but is unreadable or malformed. Treating
            // this as "no session" would overwrite it with a fresh one-message
            // session, destroying history — surface the error instead.
            crate::support::Presence::Broken(err) => return Err(err),
        };
    // `load` returns the owning plan id but does not attach the plan; re-attach
    // it so `save` keeps the plan-ownership link intact.
    if let Some(plan_id) = &plan_id
        && let Ok(storage) = crate::storage::StorageRoot::open(data_dir)
        && let Ok(plan) = storage.plans().load_current(plan_id)
    {
        chat.attach_plan(Box::new(plan));
    }
    chat.push(role, body);
    save(data_dir, session_id, created_at, &chat)
}

/// Load a session into a fresh `ChatState`. Returns the creation timestamp
/// alongside so subsequent saves keep it.
pub fn load(
    data_dir: &Path,
    session_id: &str,
) -> std::io::Result<(ChatState, chrono::DateTime<chrono::Utc>, Option<String>)> {
    let raw = std::fs::read_to_string(session_path(data_dir, session_id))?;
    let session: StoredSession = serde_json::from_str(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let mut chat = session
        .messages
        .into_iter()
        .fold(ChatState::default(), |mut chat, stored| {
            let (role, body) = to_live(stored);
            chat.push(role, body);
            chat
        });
    chat.flow = session.flow;
    Ok((chat, session.created_at, session.plan_id))
}

/// All stored sessions, most recently updated first.
pub fn list(data_dir: &Path) -> Vec<SessionSummary> {
    let dir = chats_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut sessions: Vec<SessionSummary> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| serde_json::from_str::<StoredSession>(&raw).ok())
        .map(|s| {
            let awaiting_input = stored_session_awaiting_input(&s.messages, s.flow.as_ref());
            SessionSummary {
                id: s.id,
                title: s.title,
                plan_id: s.plan_id,
                updated_at: s.updated_at,
                awaiting_input,
            }
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    sessions
}

/// Remove a stored session (missing files are fine).
pub fn delete(data_dir: &Path, session_id: &str) {
    let _ = std::fs::remove_file(session_path(data_dir, session_id));
}

/// Return the conversation owned by `plan_id`, if one exists.
pub fn find_by_plan(data_dir: &Path, plan_id: &str) -> Option<SessionSummary> {
    list(data_dir)
        .into_iter()
        .find(|session| session.plan_id.as_deref() == Some(plan_id))
}

/// Change fingerprint over exactly what [`save`] would persist: the stored
/// form of every message, the flow state, and the plan identity. Saving is
/// triggered when this differs from the last saved value, so anything that
/// changes the persisted bytes must change the fingerprint — hand-rolled
/// count/marker schemes missed in-place content changes (e.g. a run status
/// flipping without the status map growing) and skipped saves.
pub fn fingerprint(chat: &ChatState) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::hash::DefaultHasher::new();
    for stored in chat.messages.iter().filter_map(to_stored) {
        // The serialized form is what ends up on disk; a serialization
        // failure hashes as empty and the save path reports it instead.
        serde_json::to_string(&stored)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    if let Some(flow) = &chat.flow {
        serde_json::to_string(flow)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    if let Some(plan) = &chat.plan {
        plan.metadata.id.hash(&mut hasher);
        plan.metadata.version.hash(&mut hasher);
    }
    hasher.finish()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::views::chat::FlowPhase;
    use crate::plan::types::PlanMetadata;

    fn sample_chat() -> ChatState {
        let mut chat = ChatState::default();
        chat.push(
            Role::User,
            MessageBody::Text("build me a thing that does things".to_owned()),
        );
        chat.push(
            Role::Assistant,
            MessageBody::Plan {
                plan: Box::new(Plan {
                    metadata: PlanMetadata::new(Some("intent".to_owned())),
                    name: "p".to_owned(),
                    description: None,
                    inputs: vec![],
                    config: Default::default(),
                    steps: vec![],
                    outputs: vec![],
                }),
                run: Some(RunBinding {
                    run_id: "r1".to_owned(),
                    ..Default::default()
                }),
            },
        );
        chat.push(
            Role::Assistant,
            MessageBody::Human {
                prompt: "ok?".to_owned(),
                approval_required: true,
                responder: None,
                draft: String::new(),
                resolution: None,
            },
        );
        chat
    }

    #[test]
    fn save_load_round_trip_degrades_live_state() {
        let tmp = tempfile::tempdir().unwrap();
        let chat = sample_chat();
        let created = chrono::Utc::now();
        save(tmp.path(), "s1", created, &chat).unwrap();

        let (loaded, loaded_created, plan_id) = load(tmp.path(), "s1").unwrap();
        assert_eq!(loaded_created, created);
        assert!(plan_id.is_none());
        assert_eq!(loaded.messages.len(), 3);

        // The unfinished run is marked cancelled, the open prompt expired.
        match &loaded.messages[1].body {
            MessageBody::Plan { run: Some(b), .. } => {
                assert_eq!(b.finished, Some(RunStatus::Cancelled));
            }
            other => panic!(
                "expected plan message, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        match &loaded.messages[2].body {
            MessageBody::Human { resolution, .. } => assert!(resolution.is_some()),
            _ => panic!("expected human message"),
        }
    }

    /// Regression test for #108 (formerly #109): a truncated/corrupt session
    /// file must not be silently overwritten with a fresh one-message
    /// session — only a genuinely missing file should start fresh.
    #[test]
    fn append_refuses_to_overwrite_a_corrupt_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        save(tmp.path(), "corrupt", chrono::Utc::now(), &sample_chat()).unwrap();
        std::fs::write(session_path(tmp.path(), "corrupt"), b"not valid json").unwrap();

        let result = append(
            tmp.path(),
            "corrupt",
            Role::User,
            MessageBody::Text("hello".to_owned()),
        );

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(session_path(tmp.path(), "corrupt")).unwrap(),
            "not valid json",
            "the corrupt file must be left untouched, not overwritten"
        );
    }

    #[test]
    fn append_starts_a_fresh_session_when_none_exists() {
        let tmp = tempfile::tempdir().unwrap();

        append(
            tmp.path(),
            "new-session",
            Role::User,
            MessageBody::Text("hello".to_owned()),
        )
        .unwrap();

        let (loaded, _, _) = load(tmp.path(), "new-session").unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn list_orders_by_recency_and_titles_from_first_user_message() {
        let tmp = tempfile::tempdir().unwrap();
        save(tmp.path(), "a", chrono::Utc::now(), &sample_chat()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        save(tmp.path(), "b", chrono::Utc::now(), &sample_chat()).unwrap();

        let sessions = list(tmp.path());
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "b");
        assert!(sessions[0].title.starts_with("build me a thing"));
        assert!(
            sessions.iter().all(|session| session.awaiting_input),
            "an unresolved prompt must remain visible in inactive sidebar rows"
        );

        delete(tmp.path(), "b");
        assert_eq!(list(tmp.path()).len(), 1);
    }

    #[test]
    fn empty_chats_are_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        save(
            tmp.path(),
            "empty",
            chrono::Utc::now(),
            &ChatState::default(),
        )
        .unwrap();
        assert!(list(tmp.path()).is_empty());
    }

    #[test]
    fn fingerprint_changes_on_new_message_and_resolution() {
        let mut chat = sample_chat();
        let f1 = fingerprint(&chat);
        chat.push(Role::Assistant, MessageBody::Text("done".to_owned()));
        let f2 = fingerprint(&chat);
        assert_ne!(f1, f2);
        if let MessageBody::Human { resolution, .. } = &mut chat.messages[2].body {
            *resolution = Some("You approved.".to_owned());
        }
        assert_ne!(f2, fingerprint(&chat));
    }

    #[test]
    fn fingerprint_changes_when_a_step_status_flips_in_place() {
        // Regression: the old count-based fingerprint missed a status value
        // changing while the status map kept the same size, skipping saves.
        let mut chat = sample_chat();
        for message in &mut chat.messages {
            if let MessageBody::Plan { run: Some(run), .. } = &mut message.body {
                run.statuses.insert(
                    "step".to_owned(),
                    crate::storage::runs::StepRunStatus::Running,
                );
            }
        }
        let while_running = fingerprint(&chat);
        for message in &mut chat.messages {
            if let MessageBody::Plan { run: Some(run), .. } = &mut message.body {
                run.statuses.insert(
                    "step".to_owned(),
                    crate::storage::runs::StepRunStatus::Failed,
                );
            }
        }
        assert_ne!(while_running, fingerprint(&chat));
    }

    #[test]
    fn plan_ownership_round_trips_and_is_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let mut first = sample_chat();
        let owned_plan = first
            .messages
            .iter()
            .find_map(|message| match &message.body {
                MessageBody::Plan { plan, .. } => Some(plan.clone()),
                _ => None,
            });
        let owned_plan = owned_plan.expect("sample plan");
        let expected_plan_id = owned_plan.metadata.id.clone();
        let duplicate_plan = owned_plan.clone();
        first.attach_plan(owned_plan);
        save(tmp.path(), "owner", chrono::Utc::now(), &first).unwrap();

        let summary = find_by_plan(tmp.path(), &expected_plan_id).expect("owned plan chat");
        assert_eq!(summary.id, "owner");
        let (_, _, loaded_plan_id) = load(tmp.path(), "owner").unwrap();
        assert_eq!(loaded_plan_id.as_deref(), Some(expected_plan_id.as_str()));

        let mut second = sample_chat();
        second.attach_plan(duplicate_plan);
        let error = save(tmp.path(), "duplicate", chrono::Utc::now(), &second).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn guided_flow_survives_restart_with_transient_flags_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let mut chat = ChatState::default();
        chat.push(Role::User, MessageBody::Text("do a thing".to_owned()));
        let mut flow = super::super::views::chat::GuidedFlow::new("do a thing");
        flow.phase = FlowPhase::Design;
        flow.assess_turns = 2;
        flow.design_pending = true;
        flow.awaiting_compile = true;
        chat.flow = Some(flow);

        save(tmp.path(), "flowing", chrono::Utc::now(), &chat).unwrap();
        let (loaded, _, _) = load(tmp.path(), "flowing").unwrap();
        let flow = loaded.flow.expect("flow persisted");
        assert_eq!(flow.intent, "do a thing");
        assert_eq!(flow.phase, FlowPhase::Design);
        assert_eq!(flow.assess_turns, 2);
        assert_eq!(flow.conversation.len(), 1);
        // In-flight flags belong to a dead process and must reset.
        assert!(!flow.design_pending);
        assert!(!flow.awaiting_compile);
    }

    #[test]
    fn listed_design_approval_uses_the_attention_indicator() {
        let tmp = tempfile::tempdir().unwrap();
        let mut chat = ChatState::default();
        chat.push(Role::User, MessageBody::Text("do a thing".to_owned()));
        let mut flow = super::super::views::chat::GuidedFlow::new("do a thing");
        flow.phase = FlowPhase::Design;
        flow.design = Some(crate::compiler::SolutionDesign {
            title: "A design".to_owned(),
            summary: "Do the thing".to_owned(),
            recommended_tools: vec![],
            execution_outline: vec![],
        });
        chat.flow = Some(flow);

        save(tmp.path(), "approval", chrono::Utc::now(), &chat).unwrap();

        let session = list(tmp.path()).pop().expect("stored session");
        assert!(
            session.awaiting_input,
            "inactive chats awaiting design approval need attention"
        );
    }

    #[test]
    fn open_flow_prompt_survives_restart_still_answerable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut chat = ChatState::default();
        chat.push(Role::User, MessageBody::Text("do a thing".to_owned()));
        chat.push(
            Role::Assistant,
            MessageBody::FlowPrompt {
                kind: FlowPromptKind::ContinueGate,
                prompt: "The spec looks ready.".to_owned(),
                resolution: None,
            },
        );
        chat.flow = Some(super::super::views::chat::GuidedFlow::new("do a thing"));
        save(tmp.path(), "prompted", chrono::Utc::now(), &chat).unwrap();

        let (loaded, _, _) = load(tmp.path(), "prompted").unwrap();
        match &loaded.messages[1].body {
            MessageBody::FlowPrompt {
                kind, resolution, ..
            } => {
                assert_eq!(*kind, FlowPromptKind::ContinueGate);
                // Unlike Human prompts, no channel died — stays answerable.
                assert!(resolution.is_none());
            }
            _ => panic!("expected flow prompt"),
        }
        assert!(loaded.flow.is_some(), "flow context restored alongside");
    }

    #[test]
    fn fingerprint_changes_as_the_flow_advances() {
        let mut chat = sample_chat();
        let without_flow = fingerprint(&chat);
        chat.flow = Some(super::super::views::chat::GuidedFlow::new("do a thing"));
        let with_flow = fingerprint(&chat);
        assert_ne!(without_flow, with_flow);
        if let Some(flow) = chat.flow.as_mut() {
            flow.conversation.push(crate::compiler::SpecTurn {
                role: "assistant".to_owned(),
                content: "which thing?".to_owned(),
            });
        }
        assert_ne!(with_flow, fingerprint(&chat));
    }

    #[test]
    fn completed_run_action_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut chat = ChatState::default();
        chat.push(Role::User, MessageBody::Text("run it".to_owned()));
        chat.push(
            Role::Assistant,
            MessageBody::RunCompleted {
                run_id: "run-123".to_owned(),
                text: "Run succeeded in 42 ms.".to_owned(),
            },
        );
        save(tmp.path(), "completed", chrono::Utc::now(), &chat).unwrap();

        let (loaded, _, _) = load(tmp.path(), "completed").unwrap();
        assert!(matches!(
            &loaded.messages[1].body,
            MessageBody::RunCompleted { run_id, .. } if run_id == "run-123"
        ));
    }

    #[test]
    fn started_run_details_link_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut chat = ChatState::default();
        chat.push(
            Role::Assistant,
            MessageBody::RunStarted {
                run_id: "run-456".to_owned(),
                text: "Run has started.".to_owned(),
                active: true,
            },
        );
        save(tmp.path(), "started", chrono::Utc::now(), &chat).unwrap();

        let (loaded, _, _) = load(tmp.path(), "started").unwrap();
        assert!(matches!(
            &loaded.messages[0].body,
            MessageBody::RunStarted { run_id, .. } if run_id == "run-456"
        ));
    }

    #[test]
    fn agent_transcript_survives_restart_losslessly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut chat = ChatState::default();
        chat.push(
            Role::Assistant,
            MessageBody::AgentTranscript {
                run_id: "run-agent".to_owned(),
                step_id: "implement".to_owned(),
                lines: vec![
                    AgentTranscriptLine {
                        stream: super::super::views::chat::AgentTranscriptLineStream::Output,
                        content: "tool call".to_owned(),
                    },
                    AgentTranscriptLine {
                        stream: super::super::views::chat::AgentTranscriptLineStream::Error,
                        content: "diagnostic".to_owned(),
                    },
                ],
            },
        );
        save(tmp.path(), "agent", chrono::Utc::now(), &chat).unwrap();

        let (loaded, _, _) = load(tmp.path(), "agent").unwrap();
        let MessageBody::AgentTranscript { lines, .. } = &loaded.messages[0].body else {
            panic!("expected transcript")
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "tool call");
        assert_eq!(
            lines[1].stream,
            super::super::views::chat::AgentTranscriptLineStream::Error
        );
    }
}
