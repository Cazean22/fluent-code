use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fluent_code_provider::{ProviderClient, ProviderEvent};
use tracing::{Instrument, debug, info, info_span, warn};
use uuid::Uuid;

use crate::app::{Effect, Msg};
use crate::tool::execute_built_in_tool;

#[derive(Debug, Clone)]
pub struct Runtime {
    provider: ProviderClient,
    tasks: Arc<Mutex<HashMap<Uuid, tokio::task::JoinHandle<()>>>>,
}

impl Runtime {
    pub fn new(provider: ProviderClient) -> Self {
        info!(component = "runtime", "runtime orchestrator created");
        Self {
            provider,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn spawn_effect(&self, effect: Effect, sender: tokio::sync::mpsc::UnboundedSender<Msg>) {
        match effect {
            Effect::PersistSession => {
                debug!(
                    effect_kind = "persist_session",
                    "runtime received persistence effect for tui handling"
                );
            }
            Effect::PersistSessionIfDue => {
                debug!(
                    effect_kind = "persist_session_if_due",
                    "runtime received checkpoint persistence effect for tui handling"
                );
            }
            Effect::CancelAssistant { run_id } => {
                info!(run_id = %run_id, "cancel requested for assistant run");
                if let Some(handle) = self
                    .tasks
                    .lock()
                    .expect("runtime task registry lock")
                    .remove(&run_id)
                {
                    handle.abort();
                    info!(run_id = %run_id, "aborted assistant task");
                } else {
                    debug!(run_id = %run_id, "cancel requested for non-active run");
                }
            }
            Effect::StartAssistant { run_id, request } => {
                let request_message_count = request.messages.len();
                let request_tool_count = request.tools.len();
                info!(
                    run_id = %run_id,
                    request_message_count,
                    request_tool_count,
                    "spawning assistant stream task"
                );
                let provider = self.provider.clone();
                let tasks = Arc::clone(&self.tasks);
                let assistant_span = info_span!(
                    "assistant_run",
                    run_id = %run_id,
                    request_message_count,
                    request_tool_count
                );

                let handle = tokio::spawn(
                    async move {
                        let mut saw_tool_call = false;
                        debug!(task_event = "started", "assistant stream task started");
                        let result = provider
                            .stream(&request, |event| match event {
                                ProviderEvent::TextDelta(delta) => {
                                    debug!(chunk_bytes = delta.len(), "assistant chunk received");
                                    let _ = sender.send(Msg::AssistantChunk { run_id, delta });
                                }
                                ProviderEvent::ToolCall(tool_call) => {
                                    saw_tool_call = true;
                                    info!(
                                        tool_name = %tool_call.name,
                                        tool_call_id = %tool_call.id,
                                        "assistant requested tool execution"
                                    );
                                    let _ =
                                        sender.send(Msg::AssistantToolCall { run_id, tool_call });
                                }
                            })
                            .await;

                        let message = match result {
                            Ok(()) if saw_tool_call => {
                                info!(
                                    task_event = "paused_for_tool_call",
                                    "assistant stream paused after tool call"
                                );
                                None
                            }
                            Ok(()) => {
                                info!(
                                    task_event = "completed",
                                    "assistant stream finished successfully"
                                );
                                Some(Msg::AssistantDone { run_id })
                            }
                            Err(error) => {
                                warn!(error = %error, "assistant stream failed");
                                Some(Msg::AssistantFailed {
                                    run_id,
                                    error: error.to_string(),
                                })
                            }
                        };

                        if let Some(message) = message {
                            let _ = sender.send(message);
                        }

                        tasks
                            .lock()
                            .expect("runtime task registry lock")
                            .remove(&run_id);
                        debug!(
                            task_event = "removed_from_registry",
                            "assistant task removed from registry"
                        );
                    }
                    .instrument(assistant_span.or_current()),
                );

                self.tasks
                    .lock()
                    .expect("runtime task registry lock")
                    .insert(run_id, handle);
                debug!(run_id = %run_id, "assistant task inserted into registry");
            }
            Effect::ExecuteTool {
                run_id,
                invocation_id,
                tool_call,
            } => {
                info!(
                    run_id = %run_id,
                    invocation_id = %invocation_id,
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id,
                    "spawning built-in tool execution"
                );
                let tool_execution_span = info_span!(
                    "tool_execution",
                    run_id = %run_id,
                    invocation_id = %invocation_id,
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id
                );
                tokio::spawn(
                    async move {
                        debug!(task_event = "started", "tool execution task started");
                        let result =
                            execute_built_in_tool(&tool_call).map_err(|error| error.to_string());
                        match &result {
                            Ok(output) => {
                                info!(output_bytes = output.len(), "tool execution completed")
                            }
                            Err(error) => warn!(error = %error, "tool execution failed"),
                        }
                        let _ = sender.send(Msg::ToolExecutionFinished {
                            run_id,
                            invocation_id,
                            result,
                        });
                    }
                    .instrument(tool_execution_span.or_current()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Runtime;
    use crate::app::{Effect, Msg};
    use fluent_code_provider::{
        MockProvider, ProviderClient, ProviderMessage, ProviderRequest, ProviderToolCall,
    };

    #[tokio::test]
    async fn mock_provider_streams_assistant_messages() {
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            tokio::time::Duration::from_millis(5),
        )));
        let run_id = uuid::Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        runtime.spawn_effect(
            Effect::StartAssistant {
                run_id,
                request: ProviderRequest::new(
                    vec![ProviderMessage::UserText {
                        text: "hello".to_string(),
                    }],
                    vec![],
                ),
            },
            tx,
        );

        let mut messages = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Some(message)) => {
                    let is_done = matches!(message, Msg::AssistantDone { .. });
                    messages.push(message);
                    if is_done {
                        break;
                    }
                }
                _ => break,
            }
        }

        assert!(matches!(messages.first(), Some(Msg::AssistantChunk { .. })));
        assert!(
            messages
                .iter()
                .filter(|message| matches!(message, Msg::AssistantChunk { .. }))
                .count()
                >= 2
        );
        assert!(matches!(messages.last(), Some(Msg::AssistantDone { .. })));
    }

    #[tokio::test]
    async fn mock_provider_surfaces_tool_calls() {
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            tokio::time::Duration::from_millis(5),
        )));
        let run_id = uuid::Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        runtime.spawn_effect(
            Effect::StartAssistant {
                run_id,
                request: ProviderRequest::new(
                    vec![ProviderMessage::UserText {
                        text: "please use uppercase_text: hello world".to_string(),
                    }],
                    crate::tool::built_in_tools(),
                ),
            },
            tx,
        );

        let mut saw_tool_call = false;
        let mut saw_terminal_message = false;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Some(Msg::AssistantToolCall { tool_call, .. })) => {
                    assert_eq!(tool_call.name, "uppercase_text");
                    saw_tool_call = true;
                }
                Ok(Some(Msg::AssistantDone { .. } | Msg::AssistantFailed { .. })) => {
                    saw_terminal_message = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        assert!(saw_tool_call, "expected a tool call from mock provider");
        assert!(
            !saw_terminal_message,
            "tool-call pass should pause instead of emitting a terminal message"
        );
    }

    #[tokio::test]
    async fn execute_tool_effect_returns_result_message() {
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            tokio::time::Duration::from_millis(5),
        )));
        let run_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        runtime.spawn_effect(
            Effect::ExecuteTool {
                run_id,
                invocation_id,
                tool_call: ProviderToolCall {
                    id: "tool-call-1".to_string(),
                    name: "uppercase_text".to_string(),
                    arguments: serde_json::json!({ "text": "hello" }),
                },
            },
            tx,
        );

        let message = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("receive tool execution result")
            .expect("tool execution message");

        assert!(matches!(
            message,
            Msg::ToolExecutionFinished {
                run_id: received_run_id,
                invocation_id: received_invocation_id,
                result: Ok(ref output),
            } if received_run_id == run_id && received_invocation_id == invocation_id && output == "HELLO"
        ));
    }

    #[tokio::test]
    async fn cancel_aborts_active_stream_task() {
        let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
            tokio::time::Duration::from_millis(100),
        )));
        let run_id = uuid::Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        runtime.spawn_effect(
            Effect::StartAssistant {
                run_id,
                request: ProviderRequest::new(
                    vec![ProviderMessage::UserText {
                        text: "cancel me".to_string(),
                    }],
                    vec![],
                ),
            },
            tx,
        );

        let first = tokio::time::timeout(tokio::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("receive first chunk")
            .expect("chunk message");
        assert!(matches!(first, Msg::AssistantChunk { .. }));

        runtime.spawn_effect(
            Effect::CancelAssistant { run_id },
            tokio::sync::mpsc::unbounded_channel().0,
        );

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Some(Msg::AssistantDone { .. } | Msg::AssistantFailed { .. })) => {
                    panic!("expected canceled task not to emit terminal message")
                }
                Ok(Some(Msg::AssistantChunk { .. })) => continue,
                Ok(Some(Msg::AssistantToolCall { .. })) => continue,
                Ok(None) | Err(_) => break,
                _ => break,
            }
        }
    }
}
