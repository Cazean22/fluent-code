use futures::StreamExt;
use rig::{
    OneOrMany,
    client::CompletionClient,
    completion::{
        AssistantContent, CompletionModel as RigCompletionModel, Message, ToolDefinition,
    },
    providers::openai,
    streaming::StreamedAssistantContent,
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
        let client = openai::CompletionsClient::builder()
            .api_key(&api_key)
            .base_url(base_url)
            .build()
            .map_err(|error| ProviderError::Message(error.to_string()))?;
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
        let system_prompt = request
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| self.system_prompt.clone());

        let mut stream = RigCompletionModel::completion_request(&model, to_rig_message(prompt))
            .preamble(system_prompt)
            .messages(history.iter().map(to_rig_message).collect())
            .tools(tool_definitions)
            .stream()
            .await
            .map_err(|error| ProviderError::Message(error.to_string()))?;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(StreamedAssistantContent::Text(text)) => {
                    debug!(
                        provider = "openai",
                        chunk_bytes = text.text.len(),
                        "openai provider emitted text delta"
                    );
                    on_event(ProviderEvent::TextDelta(text.text))
                }
                Ok(StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                    validate_openai_tool_call_id(&tool_call.function.name, &tool_call.id)?;
                    info!(provider = "openai", tool_name = %tool_call.function.name, tool_call_id = %tool_call.id, "openai provider emitted tool call");
                    on_event(ProviderEvent::ToolCall(ProviderToolCall {
                        id: tool_call.id,
                        name: tool_call.function.name,
                        arguments: tool_call.function.arguments,
                    }));
                }
                Ok(StreamedAssistantContent::ToolCallDelta { .. })
                | Ok(StreamedAssistantContent::Reasoning(_))
                | Ok(StreamedAssistantContent::ReasoningDelta { .. })
                | Ok(StreamedAssistantContent::Final(_)) => {}
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

fn validate_openai_tool_call_id(tool_name: &str, id: &str) -> Result<()> {
    if id.trim().is_empty() {
        warn!(
            provider = "openai",
            tool_name = %tool_name,
            "openai stream emitted tool call with empty id"
        );
        return Err(ProviderError::Message(format!(
            "openai stream emitted tool call '{tool_name}' with empty id"
        )));
    }

    Ok(())
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
            id: None,
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

#[cfg(test)]
mod tests {
    use super::validate_openai_tool_call_id;
    use crate::ProviderError;

    #[test]
    fn validate_openai_tool_call_id_accepts_non_empty_id() {
        assert!(validate_openai_tool_call_id("read", "call_123").is_ok());
    }

    #[test]
    fn validate_openai_tool_call_id_rejects_empty_id() {
        let error = validate_openai_tool_call_id("read", "")
            .expect_err("empty tool call id should be rejected");

        assert!(matches!(
            error,
            ProviderError::Message(message)
                if message == "openai stream emitted tool call 'read' with empty id"
        ));
    }

    #[test]
    fn validate_openai_tool_call_id_rejects_whitespace_only_id() {
        let error = validate_openai_tool_call_id("glob", "   ")
            .expect_err("whitespace-only tool call id should be rejected");

        assert!(matches!(
            error,
            ProviderError::Message(message)
                if message == "openai stream emitted tool call 'glob' with empty id"
        ));
    }
}
