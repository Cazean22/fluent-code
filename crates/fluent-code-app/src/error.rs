use thiserror::Error;

pub type Result<T> = std::result::Result<T, FluentCodeError>;

#[derive(Debug, Error)]
pub enum FluentCodeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("invalid session data: {0}")]
    Session(String),
}

impl From<fluent_code_provider::ProviderError> for FluentCodeError {
    fn from(value: fluent_code_provider::ProviderError) -> Self {
        match value {
            fluent_code_provider::ProviderError::UnsupportedProvider(message) => {
                Self::Config(message)
            }
            fluent_code_provider::ProviderError::MissingApiKey(message) => Self::Config(message),
            fluent_code_provider::ProviderError::Message(message) => Self::Provider(message),
        }
    }
}
