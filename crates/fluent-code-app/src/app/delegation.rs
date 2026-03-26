use chrono::Utc;
use fluent_code_provider::ProviderToolCall;
use tracing::{info, warn};
use uuid::Uuid;

use crate::agent::parse_task_request;
use crate::app::request_builder::{build_provider_request, child_provider_request};
use crate::app::{AppState, AppStatus, Effect, Msg};
use crate::session::model::{Role, RunStatus, ToolExecutionState, ToolInvocationId, Turn};

#[derive(Debug, Clone)]
pub enum ChildRunOutcome {
    Completed,
    Failed { error: String },
}

pub fn start_child_run(
    state: &mut AppState,
    parent_run_id: Uuid,
    invocation_id: ToolInvocationId,
    tool_call: &ProviderToolCall,
) -> Vec<Effect> {
    let session_id = state.session.id;
    let task_request = match parse_task_request(&state.agent_registry, &tool_call.arguments) {
        Ok(task_request) => task_request,
        Err(error) => {
            finish_task_invocation_with_error(state, invocation_id, &error.to_string());
            state.status = AppStatus::Generating;
            let request = build_provider_request(state, parent_run_id);
            warn!(
                session_id = %session_id,
                run_id = %parent_run_id,
                invocation_id = %invocation_id,
                error = %error,
                "task invocation could not be delegated"
            );
            return vec![
                Effect::PersistSession,
                Effect::StartAssistant {
                    run_id: parent_run_id,
                    request,
                },
            ];
        }
    };

    let Some(agent) = state.agent_registry.get(&task_request.agent) else {
        let error_message = format!("task requested unknown agent '{}'", task_request.agent);
        finish_task_invocation_with_error(state, invocation_id, &error_message);
        state.status = AppStatus::Generating;
        let request = build_provider_request(state, parent_run_id);
        warn!(
            session_id = %session_id,
            run_id = %parent_run_id,
            invocation_id = %invocation_id,
            agent = %task_request.agent,
            "task invocation referenced an unknown agent"
        );
        return vec![
            Effect::PersistSession,
            Effect::StartAssistant {
                run_id: parent_run_id,
                request,
            },
        ];
    };

    let child_run_id = Uuid::new_v4();
    if let Some(invocation) = state.session.find_tool_invocation_mut(invocation_id) {
        invocation.set_task_delegation(
            child_run_id,
            task_request.agent.clone(),
            task_request.prompt.clone(),
        );
    }

    state.session.upsert_run_with_parent(
        child_run_id,
        RunStatus::InProgress,
        Some(parent_run_id),
        Some(invocation_id),
    );
    state.active_run_id = Some(child_run_id);
    state.status = AppStatus::Generating;

    state.session.turns.push(Turn {
        id: Uuid::new_v4(),
        run_id: child_run_id,
        role: Role::User,
        content: task_request.prompt.clone(),
        reasoning: String::new(),
        timestamp: Utc::now(),
    });
    state.session.updated_at = Utc::now();

    let child_request =
        child_provider_request(state, task_request.prompt, agent.system_prompt.clone());

    info!(
        session_id = %session_id,
        parent_run_id = %parent_run_id,
        child_run_id = %child_run_id,
        invocation_id = %invocation_id,
        agent = %agent.name,
        "started delegated child run in foreground"
    );

    vec![
        Effect::PersistSession,
        Effect::StartAssistant {
            run_id: child_run_id,
            request: child_request,
        },
    ]
}

pub fn complete_child_run(
    state: &mut AppState,
    child_run_id: Uuid,
    outcome: ChildRunOutcome,
) -> Option<Vec<Effect>> {
    let parent_link = state
        .session
        .find_run(child_run_id)
        .and_then(|run| Some((run.parent_run_id?, run.parent_tool_invocation_id?)));

    let (parent_run_id, parent_tool_invocation_id) = parent_link?;
    let session_id = state.session.id;

    let (child_status, synthetic_result) = match outcome {
        ChildRunOutcome::Completed => {
            let final_text = latest_assistant_text_for_run(state, child_run_id).unwrap_or_default();
            (RunStatus::Completed, summarize_child_result(&final_text))
        }
        ChildRunOutcome::Failed { error } => {
            let final_text = latest_assistant_text_for_run(state, child_run_id).unwrap_or_default();
            let message = if final_text.trim().is_empty() {
                format!("Subagent failed: {error}")
            } else {
                format!(
                    "Subagent failed after replying: {}",
                    summarize_child_result(&final_text)
                )
            };
            (RunStatus::Failed, message)
        }
    };

    if let Some(run) = state.session.find_run_mut(child_run_id) {
        run.status = child_status;
        run.updated_at = Utc::now();
    }

    state.active_run_id = Some(parent_run_id);
    state.status = AppStatus::Generating;

    let result_effects = crate::app::update::update(
        state,
        Msg::ToolExecutionFinished {
            run_id: parent_run_id,
            invocation_id: parent_tool_invocation_id,
            result: Ok(synthetic_result),
        },
    );

    info!(
        session_id = %session_id,
        parent_run_id = %parent_run_id,
        child_run_id = %child_run_id,
        invocation_id = %parent_tool_invocation_id,
        "child run reached terminal state and parent resumed"
    );

    Some(result_effects)
}

pub fn latest_assistant_text_for_run(state: &AppState, run_id: Uuid) -> Option<String> {
    state
        .session
        .turns
        .iter()
        .rev()
        .find(|turn| turn.run_id == run_id && matches!(turn.role, Role::Assistant))
        .map(|turn| turn.content.clone())
}

fn finish_task_invocation_with_error(
    state: &mut AppState,
    invocation_id: ToolInvocationId,
    error: &str,
) {
    if let Some(invocation) = state.session.find_tool_invocation_mut(invocation_id) {
        invocation.execution_state = ToolExecutionState::Failed;
        invocation.error = Some(error.to_string());
        invocation.completed_at = Some(Utc::now());
    }
    state.session.updated_at = Utc::now();
}

fn summarize_child_result(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "Subagent finished without a final text response.".to_string()
    } else {
        format!("Subagent finished: {trimmed}")
    }
}
