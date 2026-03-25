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

        let system_prompt = request
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| self.system_prompt.clone());

        let mut completion_request =
            RigCompletionModel::completion_request(&model, to_rig_message(prompt))
                .preamble(system_prompt)
                .messages(history.iter().map(to_rig_message).collect())
                .tools(tool_definitions);

        if let Some(params) = reasoning_effort_additional_params(self.reasoning_effort.as_deref()) {
            completion_request = completion_request.additional_params(params);
        }

        let mut stream = completion_request
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
                Ok(StreamedAssistantContent::Reasoning(reasoning)) => {
                    let summary = reasoning.display_text();
                    if !summary.is_empty() {
                        debug!(
                            provider = "openai",
                            chunk_bytes = summary.len(),
                            "openai provider emitted reasoning delta"
                        );
                        on_event(ProviderEvent::ReasoningDelta(summary));
                    }
                }
                Ok(StreamedAssistantContent::ReasoningDelta { reasoning, .. }) => {
                    if !reasoning.is_empty() {
                        debug!(
                            provider = "openai",
                            chunk_bytes = reasoning.len(),
                            "openai provider emitted reasoning delta"
                        );
                        on_event(ProviderEvent::ReasoningDelta(reasoning));
                    }
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

fn parse_reasoning_effort(value: &str) -> Option<openai::responses_api::ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Some(openai::responses_api::ReasoningEffort::None),
        "minimal" => Some(openai::responses_api::ReasoningEffort::Minimal),
        "low" => Some(openai::responses_api::ReasoningEffort::Low),
        "medium" => Some(openai::responses_api::ReasoningEffort::Medium),
        "high" => Some(openai::responses_api::ReasoningEffort::High),
        "xhigh" => Some(openai::responses_api::ReasoningEffort::Xhigh),
        _ => None,
    }
}

fn reasoning_effort_additional_params(value: Option<&str>) -> Option<serde_json::Value> {
    let value = value?;
    let effort = match parse_reasoning_effort(value) {
        Some(effort) => effort,
        None => {
            warn!(
                provider = "openai",
                reasoning_effort = %value,
                "ignoring unsupported reasoning_effort setting"
            );
            return None;
        }
    };

    Some(
        openai::responses_api::AdditionalParameters {
            reasoning: Some(openai::responses_api::Reasoning::new().with_effort(effort)),
            ..Default::default()
        }
        .to_json(),
    )
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
    use super::{
        parse_reasoning_effort, reasoning_effort_additional_params, validate_openai_tool_call_id,
    };
    use crate::ProviderError;
    use rig::providers::openai;
    use serde_json::Value;

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

    #[test]
    fn parse_reasoning_effort_accepts_supported_values() {
        assert!(matches!(
            parse_reasoning_effort("none"),
            Some(openai::responses_api::ReasoningEffort::None)
        ));
        assert!(matches!(
            parse_reasoning_effort("minimal"),
            Some(openai::responses_api::ReasoningEffort::Minimal)
        ));
        assert!(matches!(
            parse_reasoning_effort("low"),
            Some(openai::responses_api::ReasoningEffort::Low)
        ));
        assert!(matches!(
            parse_reasoning_effort("medium"),
            Some(openai::responses_api::ReasoningEffort::Medium)
        ));
        assert!(matches!(
            parse_reasoning_effort("high"),
            Some(openai::responses_api::ReasoningEffort::High)
        ));
        assert!(matches!(
            parse_reasoning_effort("xhigh"),
            Some(openai::responses_api::ReasoningEffort::Xhigh)
        ));
        assert!(matches!(
            parse_reasoning_effort(" High "),
            Some(openai::responses_api::ReasoningEffort::High)
        ));
    }

    #[test]
    fn parse_reasoning_effort_rejects_unknown_value() {
        assert!(parse_reasoning_effort("turbo").is_none());
    }

    #[test]
    fn reasoning_effort_additional_params_serializes_reasoning_effort() {
        let params = reasoning_effort_additional_params(Some("medium"))
            .expect("supported reasoning effort should produce additional params");

        assert_eq!(
            params["reasoning"]["effort"],
            Value::String("medium".to_string())
        );
    }

    #[test]
    fn reasoning_effort_additional_params_returns_none_for_none_or_invalid_value() {
        assert!(reasoning_effort_additional_params(None).is_none());
        assert!(reasoning_effort_additional_params(Some("unknown")).is_none());
    }
}
