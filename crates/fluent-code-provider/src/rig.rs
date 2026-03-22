use futures::StreamExt;
use rig::{
    OneOrMany,
    completion::{
        AssistantContent, CompletionModel as RigCompletionModel, Message, ToolDefinition,
    },
    providers::openai,
    streaming::StreamingChoice,
};
use tracing::{debug, info, warn};

use super::{
    ProviderConfig, ProviderError, ProviderEvent, ProviderMessage, ProviderRequest,
    ProviderToolCall, Result,
};

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct RigOpenAiProvider {
    model: String,
    system_prompt: String,
    reasoning_effort: Option<String>,
    provider_config: ProviderConfig,
}

impl RigOpenAiProvider {
    pub fn new(
        model: String,
        system_prompt: String,
        reasoning_effort: Option<String>,
        provider_config: ProviderConfig,
    ) -> Self {
        Self {
            model,
            system_prompt,
            reasoning_effort,
            provider_config,
        }
    }

    pub async fn stream<F>(&self, request: &ProviderRequest, mut on_event: F) -> Result<()>
    where
        F: FnMut(ProviderEvent) + Send,
    {
        info!(
            provider = "openai",
            model = %self.model,
            request_message_count = request.messages.len(),
            request_tool_count = request.tools.len(),
            "openai provider stream started"
        );
        let Some((prompt, history)) = request.messages.split_last() else {
            return Err(ProviderError::Message(
                "provider request requires at least one message".to_string(),
            ));
        };

        let api_key = resolve_api_key(&self.provider_config)?;
        let base_url = self
            .provider_config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_OPENAI_BASE_URL);
        let client = openai::Client::from_url(&api_key, base_url);
        let model = client.completion_model(&self.model);
        let tool_definitions = request
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            })
            .collect::<Vec<_>>();

        let _reasoning_effort = &self.reasoning_effort;

        let mut stream = RigCompletionModel::completion_request(&model, to_rig_message(prompt))
            .preamble(self.system_prompt.clone())
            .messages(history.iter().map(to_rig_message).collect())
            .tools(tool_definitions)
            .stream()
            .await
            .map_err(|error| ProviderError::Message(error.to_string()))?;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(StreamingChoice::Message(text)) => {
                    debug!(
                        provider = "openai",
                        chunk_bytes = text.len(),
                        "openai provider emitted text delta"
                    );
                    on_event(ProviderEvent::TextDelta(text))
                }
                Ok(StreamingChoice::ToolCall(name, id, arguments)) => {
                    info!(provider = "openai", tool_name = %name, tool_call_id = %id, "openai provider emitted tool call");
                    on_event(ProviderEvent::ToolCall(ProviderToolCall {
                        id,
                        name,
                        arguments,
                    }));
                }
                Err(error) => {
                    warn!(provider = "openai", error = %error, "openai provider stream failed");
                    return Err(ProviderError::Message(error.to_string()));
                }
            }
        }

        info!(provider = "openai", model = %self.model, "openai provider stream finished successfully");
        Ok(())
    }
}

fn to_rig_message(message: &ProviderMessage) -> Message {
    match message {
        ProviderMessage::UserText { text } => Message::user(text.clone()),
        ProviderMessage::AssistantText { text } => Message::assistant(text.clone()),
        ProviderMessage::AssistantToolCall {
            id,
            name,
            arguments,
        } => Message::Assistant {
            content: OneOrMany::one(AssistantContent::tool_call(
                id.clone(),
                name.clone(),
                arguments.clone(),
            )),
        },
        ProviderMessage::ToolResult {
            tool_call_id,
            content,
        } => Message::tool_result(tool_call_id.clone(), content.clone()),
    }
}

fn resolve_api_key(provider_config: &ProviderConfig) -> Result<String> {
    if let Some(api_key) = provider_config.api_keys.first() {
        return Ok(api_key.clone());
    }

    if provider_config.api_key_envs.is_empty() {
        return std::env::var("OPENAI_API_KEY")
            .map_err(|_| ProviderError::MissingApiKey("OPENAI_API_KEY".to_string()));
    }

    for env_name in &provider_config.api_key_envs {
        if let Ok(value) = std::env::var(env_name) {
            return Ok(value);
        }
    }

    Err(ProviderError::MissingApiKey(
        provider_config.api_key_envs.join(", "),
    ))
}
