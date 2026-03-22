use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tracing::{debug, info};

use crate::RigOpenAiProvider;

pub type Result<T> = std::result::Result<T, ProviderError>;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unsupported provider '{0}', expected 'mock' or 'openai'")]
    UnsupportedProvider(String),

    #[error("provider error: {0}")]
    Message(String),

    #[error("missing API key from configured env vars: {0}")]
    MissingApiKey(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub wire_api: Option<WireApi>,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub api_key_envs: Vec<String>,
    pub chunk_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    #[default]
    Responses,
}

#[derive(Debug, Clone)]
pub enum ProviderClient {
    Mock(MockProvider),
    OpenAi(RigOpenAiProvider),
}

impl ProviderClient {
    pub fn new(
        provider: &str,
        model: String,
        system_prompt: String,
        reasoning_effort: Option<String>,
        provider_config: Option<ProviderConfig>,
    ) -> Result<Self> {
        match provider {
            "mock" => {
                info!(provider = "mock", "creating provider client");
                Ok(Self::Mock(MockProvider::new(provider_config.as_ref())))
            }
            "openai" => {
                info!(provider = "openai", model = %model, has_reasoning_effort = reasoning_effort.is_some(), "creating provider client");
                Ok(Self::OpenAi(RigOpenAiProvider::new(
                    model,
                    system_prompt,
                    reasoning_effort,
                    provider_config.unwrap_or_default(),
                )))
            }
            other => Err(ProviderError::UnsupportedProvider(other.to_string())),
        }
    }

    pub async fn stream<F>(&self, request: &ProviderRequest, on_event: F) -> Result<()>
    where
        F: FnMut(ProviderEvent) + Send,
    {
        debug!(
            message_count = request.messages.len(),
            tool_count = request.tools.len(),
            "provider stream requested"
        );
        match self {
            Self::Mock(provider) => provider.stream(request, on_event).await,
            Self::OpenAi(provider) => provider.stream(request, on_event).await,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderRequest {
    #[serde(default)]
    pub messages: Vec<ProviderMessage>,
    #[serde(default)]
    pub tools: Vec<ProviderTool>,
}

impl ProviderRequest {
    pub fn new(messages: Vec<ProviderMessage>, tools: Vec<ProviderTool>) -> Self {
        Self { messages, tools }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderMessage {
    UserText {
        text: String,
    },
    AssistantText {
        text: String,
    },
    AssistantToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolCall(ProviderToolCall),
}

#[derive(Debug, Clone, Copy)]
pub struct MockProvider {
    chunk_delay: Duration,
}

impl MockProvider {
    pub fn new(provider_config: Option<&ProviderConfig>) -> Self {
        let chunk_delay = provider_config
            .and_then(|config| config.chunk_delay_ms)
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(25));

        Self { chunk_delay }
    }

    pub fn with_chunk_delay(chunk_delay: Duration) -> Self {
        Self { chunk_delay }
    }

    pub async fn stream<F>(&self, request: &ProviderRequest, mut on_event: F) -> Result<()>
    where
        F: FnMut(ProviderEvent) + Send,
    {
        info!(
            message_count = request.messages.len(),
            tool_count = request.tools.len(),
            "mock provider stream started"
        );
        let Some(message) = request.messages.last() else {
            return Err(ProviderError::Message(
                "provider request requires at least one message".to_string(),
            ));
        };

        match message {
            ProviderMessage::UserText { text } => {
                if let Some(tool_input) = extract_uppercase_request(text, &request.tools) {
                    let prelude = "Mock assistant: requesting uppercase_text tool. ";
                    for chunk in chunk_text(prelude) {
                        on_event(ProviderEvent::TextDelta(chunk));
                        tokio::time::sleep(self.chunk_delay).await;
                    }

                    on_event(ProviderEvent::ToolCall(ProviderToolCall {
                        id: "mock-tool-call-1".to_string(),
                        name: "uppercase_text".to_string(),
                        arguments: json!({ "text": tool_input }),
                    }));

                    info!("mock provider stream finished with tool call");
                    Ok(())
                } else {
                    let response = format!("Mock assistant response: {text}");

                    for chunk in chunk_text(&response) {
                        on_event(ProviderEvent::TextDelta(chunk));
                        tokio::time::sleep(self.chunk_delay).await;
                    }

                    info!("mock provider stream finished successfully");
                    Ok(())
                }
            }
            ProviderMessage::ToolResult { content, .. } => {
                let response = format!("Mock assistant response after tool: {content}");

                for chunk in chunk_text(&response) {
                    on_event(ProviderEvent::TextDelta(chunk));
                    tokio::time::sleep(self.chunk_delay).await;
                }

                info!("mock provider resumed after tool result and finished successfully");
                Ok(())
            }
            ProviderMessage::AssistantText { .. } | ProviderMessage::AssistantToolCall { .. } => {
                Err(ProviderError::Message(
                    "mock provider expected a user-originated final message".to_string(),
                ))
            }
        }
    }
}

fn extract_uppercase_request(prompt: &str, tools: &[ProviderTool]) -> Option<String> {
    if !tools.iter().any(|tool| tool.name == "uppercase_text") {
        return None;
    }

    let marker = "use uppercase_text:";
    let (_, requested_text) = prompt.split_once(marker)?;
    let requested_text = requested_text.trim();
    if requested_text.is_empty() {
        return None;
    }

    Some(requested_text.to_string())
}

fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();

    for segment in text.split_inclusive(' ') {
        chunks.push(segment.to_string());
    }

    if chunks.is_empty() {
        chunks.push(text.to_string());
    }

    chunks
}
