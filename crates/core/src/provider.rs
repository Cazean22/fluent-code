use std::sync::Arc;

use futures::StreamExt;
use rig::{
    OneOrMany,
    agent::Agent,
    client::CompletionClient,
    message::{Message, Text, UserContent},
    providers::openai::{
        self, responses_api::{
            AdditionalParameters, Reasoning, ReasoningEffort, ResponsesCompletionModel, streaming::StreamingCompletionResponse,
        }
    },
    streaming::{StreamedAssistantContent, StreamingCompletion},
};
use tokio::sync::mpsc::UnboundedSender;

pub type StreamContent = StreamedAssistantContent<StreamingCompletionResponse>;

pub struct OpenAIConfig {
    pub base_url: String,
    pub api_key: String,
    pub presamble: String,
    pub reason_effort: String,
    pub model: String,
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8317/v1".to_string(),
            api_key: "cazean".to_string(),
            presamble: "You are a helpful code agent.".to_string(),
            reason_effort: "xhigh".to_string(),
            model: "gpt-5.4".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct OpenAI {
    agent: Arc<Agent<ResponsesCompletionModel>>,
}

impl OpenAI {
    pub fn new(config: OpenAIConfig) -> Self {
        let client = openai::Client::builder()
            .base_url(&config.base_url)
            .api_key(&config.api_key)
            .build()
            .expect("Failed to build OpenAI client");
        let additional_params = AdditionalParameters {
            reasoning: Some(Reasoning::new().with_effort(ReasoningEffort::High)),
            ..Default::default()
        };

        let agent = client
            .agent(&config.model)
            .preamble(&config.presamble)
            .additional_params(additional_params.to_json())
            .build();

        Self {
            agent: Arc::new(agent),
        }
    }

    pub async fn run(&self, prompt: String, tx_message: UnboundedSender<StreamContent>)  {
        let mut stream = self.agent
            .stream_completion(
                Message::User {
                    content: OneOrMany::one(UserContent::Text(Text { text: prompt })),
                },
                std::iter::empty::<Message>(),
            )
            .await.expect("Failed to build completion request")
            .stream()
            .await
            .expect("Failed to stream completion");
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    let _ = tx_message.send(chunk);
                }
                Err(err) => {
                    panic!("Failed to stream completion: {}", err);
                }
            }
        }
        println!("cannot find chunk");
    }
}
