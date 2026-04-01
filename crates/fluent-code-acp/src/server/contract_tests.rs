use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use fluent_code_app::agent::AgentRegistry;
use fluent_code_app::config::{
    AcpConfig, AcpSessionDefaultsConfig, Config, LoggingConfig, LoggingFileConfig,
    LoggingStderrConfig, ModelConfig, PluginConfig,
};
use fluent_code_app::plugin::{PluginLoadSnapshot, ToolRegistry};
use fluent_code_app::runtime::Runtime;
use fluent_code_app::session::model::{
    ForegroundOwnerRecord, ForegroundPhase, Role, RunRecord, RunStatus, Session, ToolApprovalState,
    ToolExecutionState, ToolInvocationRecord, ToolPermissionAction, ToolPermissionRule,
    ToolPermissionSubject, ToolSource, Turn,
};
use fluent_code_app::session::store::{FsSessionStore, SessionStore};
use fluent_code_provider::{MockProvider, ProviderClient};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{AcpServer, AcpServerDependencies};
use crate::dev_harness::{LiveJsonlSession, ScriptedJsonlCapture, ScriptedJsonlHarness};

const SESSION_UPDATE_METHOD: &str = "session/update";
const SESSION_REQUEST_PERMISSION_METHOD: &str = "session/request_permission";
const CANCELLED_TOOL_MESSAGE: &str =
    "Tool execution was cancelled because the prompt turn was cancelled.";
const INTERRUPTED_TOOL_MESSAGE: &str = "Tool execution was interrupted during restart recovery.";

#[tokio::test]
async fn contract_initialize_rejects_unsupported_protocol_version() {
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-init-mismatch");
    fs::create_dir_all(&temp_dir).unwrap();
    let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":999}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        temp_dir.display(),
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let stdout_frames = capture.stdout_frames();

    assert_eq!(capture.frames_processed, 2);
    assert_eq!(stdout_frames.len(), 2);
    assert_eq!(stdout_frames[0]["id"], 1);
    assert_eq!(stdout_frames[0]["error"]["code"], -32602);
    assert_eq!(
        stdout_frames[0]["error"]["message"],
        "unsupported ACP protocol version `999`; expected `1`"
    );
    assert!(stdout_frames[0].get("result").is_none());
    assert_eq!(stdout_frames[1]["id"], 2);
    assert_eq!(stdout_frames[1]["error"]["code"], -32600);
    assert_eq!(
        stdout_frames[1]["error"]["message"],
        "initialize must be the first request, got `session/new`"
    );
    assert!(stdout_frames[1].get("result").is_none());
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_initialize_and_session_new_negotiate_config_over_jsonl_harness() {
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-new");
    fs::create_dir_all(&temp_dir).unwrap();
    let server = AcpServer::build(configured_test_config(
        temp_dir.clone(),
        "ACP contract prompt",
        Some("medium"),
    ))
    .unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        temp_dir.display(),
    );

    let capture = harness.run_script(&server, &script).await.unwrap();

    assert_eq!(capture.frames_processed, 2);
    assert_eq!(capture.stdout_frames().len(), 2);
    assert_eq!(
        capture.response_frame(1).unwrap()["result"]["protocolVersion"],
        1
    );
    assert_eq!(
        capture.response_frame(1).unwrap()["result"]["agentCapabilities"]["loadSession"],
        true
    );
    assert_eq!(
        config_option_current_value(capture.response_frame(2).unwrap(), "system_prompt"),
        Some("ACP contract prompt")
    );
    assert_eq!(
        config_option_current_value(capture.response_frame(2).unwrap(), "reasoning_effort"),
        Some("medium")
    );
    assert!(
        capture.response_frame(2).unwrap()["result"]
            .get("modes")
            .is_none()
    );
    assert!(
        capture
            .notification_frames(SESSION_UPDATE_METHOD)
            .is_empty()
    );
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_session_load_replays_history_and_config_over_jsonl_harness() {
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-load");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = persisted_replay_session(&store);
    let server = AcpServer::build(configured_test_config(
        temp_dir.clone(),
        "ACP load prompt",
        Some("low"),
    ))
    .unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let load_response_index = capture.frame_index_for_response(2).unwrap();
    let session_update_indices = capture.frame_indices_for_method(SESSION_UPDATE_METHOD);

    assert_eq!(capture.frames_processed, 2);
    assert_eq!(
        capture.session_update_kinds(),
        vec![
            "user_message_chunk",
            "agent_message_chunk",
            "tool_call",
            "tool_call_update",
        ]
    );
    assert_eq!(
        capture
            .notification_frames(SESSION_UPDATE_METHOD)
            .last()
            .unwrap()["params"]["update"]["rawOutput"]["result"],
        "ordered output"
    );
    assert!(!session_update_indices.is_empty());
    assert!(
        session_update_indices
            .iter()
            .all(|index| *index < load_response_index),
        "expected all session/load replay notifications before the session/load response"
    );
    assert_eq!(
        config_option_current_value(capture.response_frame(2).unwrap(), "system_prompt"),
        Some("ACP load prompt")
    );
    assert_eq!(
        config_option_current_value(capture.response_frame(2).unwrap(), "reasoning_effort"),
        Some("low")
    );
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_session_prompt_streams_tool_lifecycle_over_jsonl_harness() {
    let _guard = contract_test_lock().lock().await;
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-prompt");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let mut session = Session::new("contract prompt session");
    session.remember_tool_permission_rule(ToolPermissionRule {
        subject: ToolPermissionSubject::from_tool("uppercase_text", &ToolSource::BuiltIn),
        action: ToolPermissionAction::Allow,
    });
    store.create(&session).unwrap();

    let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"use uppercase_text: hello world\"}}]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
        session.id,
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let session_updates = capture.notification_frames(SESSION_UPDATE_METHOD);
    let update_kinds = capture.session_update_kinds();
    let first_tool_update_index = update_kinds
        .iter()
        .position(|kind| *kind == "tool_call_update")
        .unwrap();
    let resumed_agent_chunks = session_updates[first_tool_update_index + 2..]
        .iter()
        .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        capture.response_frame(3).unwrap()["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(update_kinds[0], "user_message_chunk");
    assert!(update_kinds.contains(&"tool_call"));
    assert_eq!(
        session_updates[first_tool_update_index]["params"]["update"]["status"],
        "in_progress"
    );
    assert_eq!(
        session_updates[first_tool_update_index + 1]["params"]["update"]["status"],
        "completed"
    );
    assert_eq!(
        session_updates[first_tool_update_index + 1]["params"]["update"]["rawOutput"]["result"],
        "HELLO WORLD"
    );
    assert!(!resumed_agent_chunks.is_empty());
    assert_eq!(
        resumed_agent_chunks.concat(),
        "Mock assistant response after tool: HELLO WORLD"
    );
    assert!(
        capture
            .collect_agent_message_chunks()
            .contains("Mock assistant response after tool: HELLO WORLD")
    );

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_session_load_preserves_permission_request_order_for_pending_tool_batch_over_jsonl_harness()
 {
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-permission-order");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = pending_permission_batch_session(&store);
    let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let tool_call_indices = capture.frame_indices_for_session_update_kind("tool_call");
    let permission_request_indices =
        capture.frame_indices_for_method(SESSION_REQUEST_PERMISSION_METHOD);
    let load_response_index = capture.frame_index_for_response(2).unwrap();

    assert_eq!(tool_call_indices.len(), 2);
    assert_eq!(permission_request_indices.len(), 1);
    assert_eq!(
        capture.notification_frames(SESSION_REQUEST_PERMISSION_METHOD)[0]["params"]["toolCall"]["toolCallId"],
        "glob-call-2"
    );
    assert!(
        tool_call_indices
            .iter()
            .all(|index| *index < permission_request_indices[0]),
        "expected all replayed tool_call updates before the pending permission request"
    );
    assert!(permission_request_indices[0] < load_response_index);
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_permission_round_trip_cancel_and_reload_over_jsonl_harness() {
    let _guard = contract_test_lock().lock().await;
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-permission");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = pending_permission_batch_session(&store);
    let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
    let harness = ScriptedJsonlHarness::new();
    let cancel_script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{}\"}}}}\n"
        ),
        session.id,
        temp_dir.display(),
        session.id,
    );

    let cancel_capture = harness.run_script(&server, &cancel_script).await.unwrap();
    let permission_requests = cancel_capture.notification_frames(SESSION_REQUEST_PERMISSION_METHOD);
    let load_response_index = cancel_capture.frame_index_for_response(2).unwrap();
    let permission_request_indices =
        cancel_capture.frame_indices_for_method(SESSION_REQUEST_PERMISSION_METHOD);
    let permission_option_ids = permission_requests[0]["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|option| option["optionId"].as_str())
        .collect::<Vec<_>>();

    assert_eq!(permission_requests.len(), 1);
    assert_eq!(
        cancel_capture.response_frame(2).unwrap()["result"]["latestPromptState"],
        "awaiting_tool_approval"
    );
    assert_eq!(
        permission_requests[0]["params"]["toolCall"]["toolCallId"],
        "glob-call-2"
    );
    assert_eq!(
        permission_requests[0]["params"]["toolCall"]["locations"][0]["path"],
        "/tmp/project"
    );
    assert_eq!(
        permission_option_ids,
        vec!["allow_once", "allow_always", "reject_once", "reject_always"]
    );
    assert_eq!(permission_request_indices.len(), 1);
    assert!(
        permission_request_indices[0] < load_response_index,
        "expected the pending permission request to be emitted before session/load resolves"
    );
    assert_eq!(
        cancel_capture.response_frame(3).unwrap()["result"]["stopReason"],
        "cancelled"
    );
    assert_stdout_contains_only_jsonrpc_frames(&cancel_capture);

    let reload_script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
    );
    let reload_capture = harness.run_script(&server, &reload_script).await.unwrap();
    let cancelled_tool_updates = reload_capture
        .notification_frames(SESSION_UPDATE_METHOD)
        .into_iter()
        .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "tool_call_update")
        .filter(|frame| frame["params"]["update"]["rawOutput"]["error"] == CANCELLED_TOOL_MESSAGE)
        .count();

    assert!(
        reload_capture
            .notification_frames(SESSION_REQUEST_PERMISSION_METHOD)
            .is_empty()
    );
    assert_eq!(
        reload_capture.response_frame(2).unwrap()["result"]["latestPromptState"],
        "cancelled"
    );
    assert_eq!(cancelled_tool_updates, 2);
    assert_stdout_contains_only_jsonrpc_frames(&reload_capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_interrupted_load_surfaces_terminal_state_over_jsonl_harness() {
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-interrupted-load");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = running_tool_session();
    store.create(&session).unwrap();
    let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let interrupted_update = capture
        .notification_frames(SESSION_UPDATE_METHOD)
        .into_iter()
        .find(|frame| frame["params"]["update"]["rawOutput"]["error"] == INTERRUPTED_TOOL_MESSAGE)
        .unwrap();

    assert_eq!(interrupted_update["params"]["update"]["status"], "failed");
    assert!(capture.response_frame(2).unwrap()["result"].is_object());

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_live_same_connection_cancel_resolves_prompt_over_stdio_loop() {
    let _guard = contract_test_lock().lock().await;
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-live-cancel");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = Session::new("contract live cancel session");
    let session_id = session.id;
    store.create(&session).unwrap();

    let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(50));
    let harness = ScriptedJsonlHarness::new();
    let live = harness.start_live_session(&server);

    live.send_frame(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
    )
    .unwrap();
    live.send_frame(format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}",
        session_id,
        temp_dir.display(),
    ))
    .unwrap();
    live.send_frame(format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"interrupt this prompt\"}}]}}}}",
        session_id,
    ))
    .unwrap();

    let first_agent_chunk_capture = wait_for_first_agent_chunk(&live).await;
    assert!(first_agent_chunk_capture.response_frame(3).is_none());

    live.send_frame(format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/cancel\",\"params\":{{\"sessionId\":\"{}\"}}}}",
        session_id,
    ))
    .unwrap();

    live.wait_until("cancel and prompt responses", |capture| {
        capture.response_frame(4).is_some() && capture.response_frame(3).is_some()
    })
    .await
    .unwrap();
    let capture = live.finish().await.unwrap();
    let cancel_response_index = capture.frame_index_for_response(4).unwrap();
    let prompt_response_index = capture.frame_index_for_response(3).unwrap();
    let agent_chunk_indices = capture.frame_indices_for_session_update_kind("agent_message_chunk");
    let session_update_indices = capture.frame_indices_for_method(SESSION_UPDATE_METHOD);

    assert_eq!(capture.frames_processed, 4);
    assert_eq!(capture.response_ids(), vec![1, 2, 4, 3]);
    assert!(
        capture
            .stdout_frames()
            .iter()
            .all(|frame| frame.get("error").is_none())
    );
    assert!(!agent_chunk_indices.is_empty());
    assert!(agent_chunk_indices[0] < cancel_response_index);
    assert!(agent_chunk_indices[0] < prompt_response_index);
    assert!(
        session_update_indices
            .iter()
            .all(|index| *index < cancel_response_index),
        "expected cancel during idle polling to suppress stale post-cancel session/update frames"
    );
    assert_eq!(
        capture.response_frame(4).unwrap()["result"]["stopReason"],
        "cancelled"
    );
    assert_eq!(
        capture.response_frame(3).unwrap()["result"]["stopReason"],
        "cancelled"
    );
    assert!(cancel_response_index < prompt_response_index);
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_session_prompt_flushes_first_delta_before_terminal_over_jsonl_harness() {
    let _guard = contract_test_lock().lock().await;
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-prompt-first-delta");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = Session::new("contract first delta prompt session");
    store.create(&session).unwrap();

    let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(4));
    let prompt_text = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen";
    let expected_response = format!("Mock assistant response: {prompt_text}");
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
        session.id,
        prompt_text,
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let prompt_response_index = capture.frame_index_for_response(3).unwrap();
    let agent_chunk_indices = capture.frame_indices_for_session_update_kind("agent_message_chunk");
    let agent_chunks = capture.collect_agent_message_chunk_texts();
    let emitted_prefix_length = agent_chunks[..agent_chunks.len() - 1]
        .iter()
        .map(String::len)
        .sum::<usize>();

    assert_eq!(
        capture.response_frame(3).unwrap()["result"]["stopReason"],
        "end_turn"
    );
    assert!(agent_chunks.len() >= 2);
    assert_eq!(capture.collect_agent_message_chunks(), expected_response);
    assert!(agent_chunk_indices[0] < prompt_response_index);
    assert_eq!(
        capture.stdout_frames()[prompt_response_index - 1]["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
    assert!(emitted_prefix_length > 0);
    assert!(emitted_prefix_length < expected_response.len());
    assert_eq!(
        agent_chunks.last().unwrap(),
        &expected_response[emitted_prefix_length..]
    );
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_session_prompt_preserves_chunk_continuity_without_duplicate_text_over_jsonl_harness()
 {
    let _guard = contract_test_lock().lock().await;
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-prompt-chunk-continuity");
    fs::create_dir_all(&temp_dir).unwrap();
    let store = FsSessionStore::new(temp_dir.clone());
    let session = Session::new("contract chunk continuity prompt session");
    store.create(&session).unwrap();

    let server = test_server_with_chunk_delay(temp_dir.clone(), Duration::from_millis(15));
    let prompt_text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    let expected_response = format!("Mock assistant response: {prompt_text}");
    let expected_chunks = split_text_like_mock_provider(&expected_response);
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/load\",\"params\":{{\"sessionId\":\"{}\",\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{{\"sessionId\":\"{}\",\"prompt\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n"
        ),
        session.id,
        temp_dir.display(),
        session.id,
        prompt_text,
    );

    let capture = harness.run_script(&server, &script).await.unwrap();
    let agent_chunks = capture.collect_agent_message_chunk_texts();
    let unique_agent_chunks = agent_chunks.iter().cloned().collect::<HashSet<_>>();

    assert_eq!(
        capture.response_frame(3).unwrap()["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(agent_chunks, expected_chunks);
    assert_eq!(agent_chunks.concat(), expected_response);
    assert_eq!(capture.collect_agent_message_chunks(), expected_response);
    assert_eq!(unique_agent_chunks.len(), agent_chunks.len());
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

#[tokio::test]
async fn contract_stdout_contains_only_protocol_messages_over_jsonl_harness() {
    let temp_dir = unique_temp_dir("fluent-code-acp-contract-stdout-jsonrpc");
    fs::create_dir_all(&temp_dir).unwrap();
    let server = AcpServer::build(test_config(temp_dir.clone())).unwrap();
    let harness = ScriptedJsonlHarness::new();
    let script = format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":1}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n"
        ),
        temp_dir.display(),
    );

    let capture = harness.run_script(&server, &script).await.unwrap();

    assert_eq!(capture.frames_processed, 2);
    assert_stdout_contains_only_jsonrpc_frames(&capture);

    cleanup(temp_dir);
}

fn config_option_current_value<'a>(response_frame: &'a Value, option_id: &str) -> Option<&'a str> {
    response_frame["result"]["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["id"] == option_id)
        .and_then(|option| option["currentValue"].as_str())
}

fn assert_stdout_contains_only_jsonrpc_frames(capture: &ScriptedJsonlCapture) {
    let stdout_lines = capture
        .stdout_text()
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    assert_eq!(stdout_lines.len(), capture.stdout_frames().len());
    assert!(stdout_lines.iter().all(|line| !line.contains(['\r', '\n'])));
    assert!(stdout_lines.iter().all(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|frame| {
                frame
                    .get("jsonrpc")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("2.0")
    }));
}

async fn wait_for_first_agent_chunk(live: &LiveJsonlSession) -> ScriptedJsonlCapture {
    live.wait_until("the first agent message chunk", |capture| {
        !capture.collect_agent_message_chunk_texts().is_empty()
    })
    .await
    .unwrap()
}

fn split_text_like_mock_provider(text: &str) -> Vec<String> {
    let mut chunks = text
        .split_inclusive(' ')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

fn test_config(data_dir: PathBuf) -> Config {
    configured_test_config(data_dir, "You are a helpful coding assistant.", None)
}

fn configured_test_config(
    data_dir: PathBuf,
    system_prompt: &str,
    reasoning_effort: Option<&str>,
) -> Config {
    let plugin_root = data_dir.join("plugins");
    fs::create_dir_all(plugin_root.join("project")).unwrap();
    fs::create_dir_all(plugin_root.join("global")).unwrap();

    Config {
        config_path: None,
        data_dir: data_dir.clone(),
        logging: LoggingConfig {
            file: LoggingFileConfig {
                enabled: false,
                path: data_dir.join("logs/fluent-code.log"),
                level: "info".to_string(),
            },
            stderr: LoggingStderrConfig {
                enabled: false,
                level: "info".to_string(),
            },
        },
        model: ModelConfig {
            provider: "mock".to_string(),
            model: "gpt-4.1-mini".to_string(),
            reasoning_effort: None,
            system_prompt: "You are a helpful coding assistant.".to_string(),
        },
        agents: None,
        plugins: PluginConfig {
            enable_project_plugins: false,
            enable_global_plugins: false,
            project_dir: plugin_root.join("project"),
            global_dir: plugin_root.join("global"),
        },
        acp: AcpConfig {
            protocol_version: 1,
            auth_methods: Vec::new(),
            session_defaults: AcpSessionDefaultsConfig {
                system_prompt: system_prompt.to_string(),
                reasoning_effort: reasoning_effort.map(str::to_string),
            },
        },
        model_providers: HashMap::new(),
    }
}

fn test_server_with_chunk_delay(data_dir: PathBuf, chunk_delay: Duration) -> AcpServer {
    let config = test_config(data_dir.clone());
    let store = FsSessionStore::new(data_dir);
    let agent_registry = Arc::new(AgentRegistry::built_in().clone());
    let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
    let runtime = Runtime::new_with_tool_registry(
        ProviderClient::Mock(MockProvider::with_chunk_delay(chunk_delay)),
        Arc::clone(&tool_registry),
    );

    AcpServer::from_dependencies(AcpServerDependencies {
        config,
        store,
        agent_registry,
        runtime,
        tool_registry,
        plugin_load_snapshot: PluginLoadSnapshot::default(),
    })
}

fn persisted_replay_session(store: &FsSessionStore) -> Session {
    let mut session = Session::new("ordered replay session");
    let run_id = Uuid::new_v4();
    let assistant_turn_id = Uuid::new_v4();
    let shared_timestamp = Utc::now();
    session.upsert_run(run_id, RunStatus::InProgress);

    let user_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: Uuid::new_v4(),
        run_id,
        role: Role::User,
        content: "replay this session".to_string(),
        reasoning: String::new(),
        sequence_number: user_sequence_number,
        timestamp: shared_timestamp + chrono::Duration::seconds(10),
    });

    let assistant_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: assistant_turn_id,
        run_id,
        role: Role::Assistant,
        content: "ordered answer".to_string(),
        reasoning: String::new(),
        sequence_number: assistant_sequence_number,
        timestamp: shared_timestamp - chrono::Duration::seconds(20),
    });

    let invocation_sequence_number = session.allocate_replay_sequence();
    session.tool_invocations.push(ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: "read-call-1".to_string(),
        tool_name: "read".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: serde_json::json!({ "path": "/tmp/notes.txt" }),
        preceding_turn_id: Some(assistant_turn_id),
        approval_state: ToolApprovalState::Approved,
        execution_state: ToolExecutionState::Completed,
        result: Some("ordered output".to_string()),
        error: None,
        delegation: None,
        sequence_number: invocation_sequence_number,
        requested_at: shared_timestamp - chrono::Duration::seconds(30),
        approved_at: Some(shared_timestamp - chrono::Duration::seconds(29)),
        completed_at: Some(shared_timestamp - chrono::Duration::seconds(28)),
    });

    session.upsert_run_with_stop_reason(run_id, RunStatus::Completed, None);
    store.create(&session).unwrap();
    session
}

fn pending_permission_batch_session(store: &FsSessionStore) -> Session {
    let mut session = Session::new("pending permission batch");
    let run_id = Uuid::new_v4();
    let assistant_turn_id = Uuid::new_v4();
    session.upsert_run(run_id, RunStatus::InProgress);

    let user_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: Uuid::new_v4(),
        run_id,
        role: Role::User,
        content: "inspect the repository".to_string(),
        reasoning: String::new(),
        sequence_number: user_sequence_number,
        timestamp: Utc::now(),
    });

    let assistant_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: assistant_turn_id,
        run_id,
        role: Role::Assistant,
        content: "I should inspect a few paths".to_string(),
        reasoning: String::new(),
        sequence_number: assistant_sequence_number,
        timestamp: Utc::now(),
    });

    let first_invocation_sequence_number = session.allocate_replay_sequence();
    session.tool_invocations.push(ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: "read-call-1".to_string(),
        tool_name: "read".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: serde_json::json!({ "path": "/tmp/first.txt" }),
        preceding_turn_id: Some(assistant_turn_id),
        approval_state: ToolApprovalState::Pending,
        execution_state: ToolExecutionState::NotStarted,
        result: None,
        error: None,
        delegation: None,
        sequence_number: first_invocation_sequence_number,
        requested_at: Utc::now(),
        approved_at: None,
        completed_at: None,
    });

    let second_invocation_sequence_number = session.allocate_replay_sequence();
    session.tool_invocations.push(ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: "glob-call-2".to_string(),
        tool_name: "glob".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: serde_json::json!({ "pattern": "**/*.rs", "path": "/tmp/project" }),
        preceding_turn_id: Some(assistant_turn_id),
        approval_state: ToolApprovalState::Pending,
        execution_state: ToolExecutionState::NotStarted,
        result: None,
        error: None,
        delegation: None,
        sequence_number: second_invocation_sequence_number,
        requested_at: Utc::now(),
        approved_at: None,
        completed_at: None,
    });

    session.foreground_owner = Some(ForegroundOwnerRecord {
        run_id,
        phase: ForegroundPhase::AwaitingToolApproval,
        batch_anchor_turn_id: Some(assistant_turn_id),
    });
    store.create(&session).unwrap();
    session
}

fn running_tool_session() -> Session {
    let mut session = Session::new("running tool recovery");
    let run_id = Uuid::new_v4();
    let assistant_turn_id = Uuid::new_v4();
    let run_created_sequence = session.allocate_replay_sequence();
    session.runs.push(RunRecord {
        id: run_id,
        status: RunStatus::InProgress,
        parent_run_id: None,
        parent_tool_invocation_id: None,
        created_sequence: run_created_sequence,
        terminal_sequence: None,
        terminal_stop_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    });
    let user_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: Uuid::new_v4(),
        run_id,
        role: Role::User,
        content: "read the file".to_string(),
        reasoning: String::new(),
        sequence_number: user_sequence_number,
        timestamp: Utc::now(),
    });
    let assistant_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: assistant_turn_id,
        run_id,
        role: Role::Assistant,
        content: "I will read the file".to_string(),
        reasoning: String::new(),
        sequence_number: assistant_sequence_number,
        timestamp: Utc::now(),
    });
    let invocation_sequence_number = session.allocate_replay_sequence();
    session.tool_invocations.push(ToolInvocationRecord {
        id: Uuid::new_v4(),
        run_id,
        tool_call_id: "read-call-1".to_string(),
        tool_name: "read".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: serde_json::json!({ "path": "Cargo.toml" }),
        preceding_turn_id: Some(assistant_turn_id),
        approval_state: ToolApprovalState::Approved,
        execution_state: ToolExecutionState::Running,
        result: None,
        error: None,
        delegation: None,
        sequence_number: invocation_sequence_number,
        requested_at: Utc::now(),
        approved_at: Some(Utc::now()),
        completed_at: None,
    });
    session.foreground_owner = Some(ForegroundOwnerRecord {
        run_id,
        phase: ForegroundPhase::RunningTool,
        batch_anchor_turn_id: Some(assistant_turn_id),
    });
    session
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique_suffix}"))
}

fn cleanup(path: PathBuf) {
    if path.exists() {
        fs::remove_dir_all(path).unwrap();
    }
}

fn contract_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
