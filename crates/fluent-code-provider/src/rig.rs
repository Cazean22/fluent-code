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
const OPENAI_STARTUP_STREAM_MAX_ATTEMPTS: usize = 2;
const OPENAI_EMPTY_STARTUP_STREAM_MESSAGE: &str = "openai provider stream ended before first event";

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
        let reasoning_params = reasoning_effort_additional_params(self.reasoning_effort.as_deref());

        'attempts: for attempt in 0..OPENAI_STARTUP_STREAM_MAX_ATTEMPTS {
            let mut completion_request =
                RigCompletionModel::completion_request(&model, to_rig_message(prompt))
                    .preamble(system_prompt.clone())
                    .messages(history.iter().map(to_rig_message).collect())
                    .tools(tool_definitions.clone());

            if let Some(params) = reasoning_params.clone() {
                completion_request = completion_request.additional_params(params);
            }

            let mut stream = match completion_request.stream().await {
                Ok(stream) => stream,
                Err(error) => {
                    let message = error.to_string();
                    if should_retry_openai_startup_failure(attempt, false, &message) {
                        warn!(
                            provider = "openai",
                            attempt = attempt + 1,
                            max_attempts = OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
                            error = %message,
                            "openai provider stream startup failed before first event; retrying"
                        );
                        continue;
                    }

                    let surfaced_error = format_openai_stream_failure(attempt, false, &message);
                    warn!(
                        provider = "openai",
                        attempt = attempt + 1,
                        max_attempts = OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
                        error = %surfaced_error,
                        "openai provider stream failed"
                    );
                    return Err(ProviderError::Message(surfaced_error));
                }
            };

            let mut forwarded_event = false;

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(StreamedAssistantContent::Text(text)) => {
                        forwarded_event = true;
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
                            forwarded_event = true;
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
                            forwarded_event = true;
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
                        forwarded_event = true;
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
                        let message = error.to_string();
                        if should_retry_openai_startup_failure(attempt, forwarded_event, &message) {
                            warn!(
                                    provider = "openai",
                                    attempt = attempt + 1,
                                    max_attempts = OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
                                    error = %message,
                                    "openai provider stream failed before first event; retrying"
                            );
                            continue 'attempts;
                        }

                        let surfaced_error =
                            format_openai_stream_failure(attempt, forwarded_event, &message);
                        warn!(
                            provider = "openai",
                            attempt = attempt + 1,
                            max_attempts = OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
                            error = %surfaced_error,
                            "openai provider stream failed"
                        );
                        return Err(ProviderError::Message(surfaced_error));
                    }
                }
            }

            if !forwarded_event {
                let message = OPENAI_EMPTY_STARTUP_STREAM_MESSAGE.to_string();
                if should_retry_empty_startup_stream(attempt, forwarded_event) {
                    warn!(
                        provider = "openai",
                        attempt = attempt + 1,
                        max_attempts = OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
                        error = %message,
                        "openai provider stream finished before first event; retrying"
                    );
                    continue;
                }

                let surfaced_error = format_openai_stream_failure(attempt, false, &message);
                warn!(
                    provider = "openai",
                    attempt = attempt + 1,
                    max_attempts = OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
                    error = %surfaced_error,
                    "openai provider stream failed"
                );
                return Err(ProviderError::Message(surfaced_error));
            }

            info!(provider = "openai", model = %self.model, "openai provider stream finished successfully");
            return Ok(());
        }

        unreachable!("openai startup retry loop should return or retry within max attempts")
    }
}

fn should_retry_openai_startup_failure(
    attempt: usize,
    forwarded_event: bool,
    message: &str,
) -> bool {
    should_retry_empty_startup_stream(attempt, forwarded_event)
        && is_retryable_openai_startup_error(message)
}

fn should_retry_empty_startup_stream(attempt: usize, forwarded_event: bool) -> bool {
    !forwarded_event && attempt + 1 < OPENAI_STARTUP_STREAM_MAX_ATTEMPTS
}

fn format_openai_stream_failure(attempt: usize, forwarded_event: bool, message: &str) -> String {
    let phase = if forwarded_event {
        "after first event"
    } else {
        "before first event"
    };

    format!(
        "openai provider stream failed on attempt {}/{} {}: {}",
        attempt + 1,
        OPENAI_STARTUP_STREAM_MAX_ATTEMPTS,
        phase,
        message
    )
}

fn is_retryable_openai_startup_error(message: &str) -> bool {
    let normalized = message.trim().to_ascii_lowercase();

    normalized.contains("empty_stream")
        || normalized.contains("empty stream")
        || normalized.contains("internal server error")
        || normalized.contains("bad gateway")
        || normalized.contains("service unavailable")
        || normalized.contains("gateway timeout")
        || is_http_5xx_status_error(&normalized)
}

fn is_http_5xx_status_error(message: &str) -> bool {
    if !message.contains("http") && !message.contains("status") {
        return false;
    }

    message
        .split(|character: char| !character.is_ascii_digit())
        .filter(|token| token.len() == 3)
        .filter_map(|token| token.parse::<u16>().ok())
        .any(|status| (500..600).contains(&status))
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
        OPENAI_EMPTY_STARTUP_STREAM_MESSAGE, format_openai_stream_failure,
        is_retryable_openai_startup_error, parse_reasoning_effort,
        reasoning_effort_additional_params, should_retry_empty_startup_stream,
        should_retry_openai_startup_failure, validate_openai_tool_call_id,
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

    #[test]
    fn is_retryable_openai_startup_error_accepts_narrow_transient_failures() {
        assert!(is_retryable_openai_startup_error("empty_stream"));
        assert!(is_retryable_openai_startup_error(
            "OpenAI request failed with HTTP 503 Service Unavailable"
        ));
        assert!(is_retryable_openai_startup_error(
            "response status code: 502 bad gateway"
        ));
    }

    #[test]
    fn is_retryable_openai_startup_error_rejects_non_transient_failures() {
        assert!(!is_retryable_openai_startup_error(
            "OpenAI request failed with HTTP 401 Unauthorized"
        ));
        assert!(!is_retryable_openai_startup_error(
            "OpenAI request failed with status 429 Too Many Requests"
        ));
        assert!(!is_retryable_openai_startup_error(
            "openai stream emitted tool call 'read' with empty id"
        ));
        assert!(!is_retryable_openai_startup_error(
            "invalid tool schema for function read"
        ));
    }

    #[test]
    fn should_retry_openai_startup_failure_only_before_first_forwarded_event() {
        assert!(should_retry_openai_startup_failure(
            0,
            false,
            "empty_stream"
        ));
        assert!(!should_retry_openai_startup_failure(
            0,
            true,
            "empty_stream"
        ));
    }

    #[test]
    fn should_retry_openai_startup_failure_only_once() {
        assert!(should_retry_openai_startup_failure(
            0,
            false,
            "HTTP 500 Internal Server Error"
        ));
        assert!(!should_retry_openai_startup_failure(
            1,
            false,
            "HTTP 500 Internal Server Error"
        ));
    }

    #[test]
    fn empty_startup_stream_completion_is_treated_as_failure() {
        assert!(should_retry_empty_startup_stream(0, false));
        assert!(!should_retry_empty_startup_stream(0, true));
        assert!(!should_retry_empty_startup_stream(1, false));
        assert_eq!(
            OPENAI_EMPTY_STARTUP_STREAM_MESSAGE,
            "openai provider stream ended before first event"
        );
    }

    #[test]
    fn format_openai_stream_failure_includes_attempt_context() {
        assert_eq!(
            format_openai_stream_failure(1, false, "empty_stream"),
            "openai provider stream failed on attempt 2/2 before first event: empty_stream"
        );
        assert_eq!(
            format_openai_stream_failure(0, true, "stream interrupted"),
            "openai provider stream failed on attempt 1/2 after first event: stream interrupted"
        );
    }
}
