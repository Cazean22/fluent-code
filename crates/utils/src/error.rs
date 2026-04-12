use thiserror::Error;

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

    #[error("plugin error: {0}")]
    Plugin(String),

    #[error("invalid session data: {0}")]
    Session(String),
}

pub type Result<T> = std::result::Result<T, FluentCodeError>;
