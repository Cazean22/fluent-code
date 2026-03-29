use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use fluent_code_provider::ProviderConfig;
use serde::Deserialize;

use crate::{FluentCodeError, Result};

const ROOT_CONFIG_FILE: &str = "fluent-code.toml";
const DATA_DIR_CONFIG_FILE: &str = "config.toml";
const DEFAULT_MODEL_PROVIDER: &str = "mock";
const DEFAULT_MODEL: &str = "gpt-4.1-mini";
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful coding assistant.";
const DEFAULT_ACP_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone)]
pub struct Config {
    pub config_path: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub logging: LoggingConfig,
    pub model: ModelConfig,
    pub agents: Option<Vec<AgentConfig>>,
    pub plugins: PluginConfig,
    pub acp: AcpConfig,
    pub model_providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfig {
    pub protocol_version: u16,
    pub auth_methods: Vec<AcpAuthMethodConfig>,
    pub session_defaults: AcpSessionDefaultsConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAuthMethodConfig {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSessionDefaultsConfig {
    pub system_prompt: String,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfig {
    pub enable_project_plugins: bool,
    pub enable_global_plugins: bool,
    pub project_dir: PathBuf,
    pub global_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingConfig {
    pub file: LoggingFileConfig,
    pub stderr: LoggingStderrConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingFileConfig {
    pub enabled: bool,
    pub path: PathBuf,
    pub level: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingStderrConfig {
    pub enabled: bool,
    pub level: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub system_prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub tools_allowed: Option<Vec<String>>,
    pub tools_denied: Option<Vec<String>>,
    pub delegation_targets: Option<Vec<String>>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root_config = current_dir.join(ROOT_CONFIG_FILE);
        if root_config.exists() {
            return Self::load_from_path(&root_config, &current_dir);
        }

        let data_dir_config = current_dir.join(".fluent-code").join(DATA_DIR_CONFIG_FILE);
        if data_dir_config.exists() {
            return Self::load_from_path(&data_dir_config, &current_dir);
        }

        Ok(Self::default_with_base_dir(&current_dir))
    }

    pub fn selected_provider_config(&self) -> Option<&ProviderConfig> {
        self.model_providers.get(&self.model.provider)
    }

    fn load_from_path(path: &Path, current_dir: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)?;
        let mut config = Self::from_toml_str(&contents, current_dir)?;
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    fn from_toml_str(contents: &str, current_dir: &Path) -> Result<Self> {
        let raw: RawConfig = toml::from_str(contents)?;
        let mut config = Self::default_with_base_dir(current_dir);

        if let Some(data_dir) = raw.data_dir {
            config.data_dir = resolve_path(current_dir, data_dir);
        }

        if let Some(logging) = raw.logging {
            config.logging = LoggingConfig::from_raw(logging, &config.data_dir);
        } else {
            config.logging = LoggingConfig::default_with_data_dir(&config.data_dir);
        }

        config.agents = raw
            .agents
            .map(|agents| agents.into_iter().map(AgentConfig::from_raw).collect());

        if let Some(plugins) = raw.plugins {
            config.plugins = PluginConfig::from_raw(plugins, current_dir, &config.data_dir);
        } else {
            config.plugins = PluginConfig::default_with_paths(current_dir, &config.data_dir);
        }

        if let Some(provider) = raw.model_provider {
            config.model.provider = provider;
        }

        if let Some(model) = raw.model {
            config.model.model = model;
        }

        if let Some(reasoning_effort) = raw.model_reasoning_effort {
            config.model.reasoning_effort =
                Some(validate_model_reasoning_effort(reasoning_effort)?);
        }

        if let Some(system_prompt) = raw.system_prompt {
            config.model.system_prompt = system_prompt;
        }

        config.acp = if let Some(acp) = raw.acp {
            AcpConfig::from_raw(acp, &config.model)?
        } else {
            AcpConfig::default_with_model(&config.model)
        };

        if !raw.model_providers.is_empty() {
            config.model_providers = raw.model_providers;
        }

        Ok(config)
    }

    fn default_with_base_dir(base_dir: &Path) -> Self {
        let data_dir = base_dir.join(".fluent-code");
        let model = ModelConfig {
            provider: DEFAULT_MODEL_PROVIDER.to_string(),
            model: DEFAULT_MODEL.to_string(),
            reasoning_effort: None,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        };

        Self {
            config_path: None,
            data_dir: data_dir.clone(),
            logging: LoggingConfig::default_with_data_dir(&data_dir),
            model: model.clone(),
            agents: None,
            plugins: PluginConfig::default_with_paths(base_dir, &data_dir),
            acp: AcpConfig::default_with_model(&model),
            model_providers: HashMap::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    data_dir: Option<PathBuf>,
    logging: Option<RawLoggingConfig>,
    agents: Option<Vec<RawAgentConfig>>,
    plugins: Option<RawPluginConfig>,
    acp: Option<RawAcpConfig>,
    model_provider: Option<String>,
    model: Option<String>,
    model_reasoning_effort: Option<String>,
    system_prompt: Option<String>,
    #[serde(default)]
    model_providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLoggingConfig {
    file: Option<RawLoggingFileConfig>,
    stderr: Option<RawLoggingStderrConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLoggingFileConfig {
    enabled: Option<bool>,
    path: Option<PathBuf>,
    level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLoggingStderrConfig {
    enabled: Option<bool>,
    level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPluginConfig {
    enable_project_plugins: Option<bool>,
    enable_global_plugins: Option<bool>,
    project_dir: Option<PathBuf>,
    global_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAgentConfig {
    name: String,
    description: String,
    system_prompt: String,
    #[serde(default)]
    tools_allowed: Option<Vec<String>>,
    #[serde(default)]
    tools_denied: Option<Vec<String>>,
    #[serde(default)]
    delegation_targets: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcpConfig {
    protocol_version: Option<u16>,
    #[serde(default)]
    auth_methods: Vec<RawAcpAuthMethodConfig>,
    session_defaults: Option<RawAcpSessionDefaultsConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcpAuthMethodConfig {
    id: String,
    name: String,
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcpSessionDefaultsConfig {
    system_prompt: Option<String>,
    reasoning_effort: Option<String>,
}

impl AcpConfig {
    fn default_with_model(model: &ModelConfig) -> Self {
        Self {
            protocol_version: DEFAULT_ACP_PROTOCOL_VERSION,
            auth_methods: Vec::new(),
            session_defaults: AcpSessionDefaultsConfig::default_with_model(model),
        }
    }

    fn from_raw(raw: RawAcpConfig, model: &ModelConfig) -> Result<Self> {
        let defaults = Self::default_with_model(model);

        Ok(Self {
            protocol_version: raw
                .protocol_version
                .map(validate_acp_protocol_version)
                .transpose()?
                .unwrap_or(defaults.protocol_version),
            auth_methods: raw
                .auth_methods
                .into_iter()
                .map(AcpAuthMethodConfig::from_raw)
                .collect::<Result<Vec<_>>>()?,
            session_defaults: AcpSessionDefaultsConfig::from_raw(raw.session_defaults, model)?,
        })
    }
}

impl AcpAuthMethodConfig {
    fn from_raw(raw: RawAcpAuthMethodConfig) -> Result<Self> {
        let id = raw.id.trim();
        if id.is_empty() {
            return Err(FluentCodeError::Config(
                "acp.auth_methods entries require a non-empty id".to_string(),
            ));
        }

        let name = raw.name.trim();
        if name.is_empty() {
            return Err(FluentCodeError::Config(
                "acp.auth_methods entries require a non-empty name".to_string(),
            ));
        }

        Ok(Self {
            id: id.to_string(),
            name: name.to_string(),
            description: raw.description,
        })
    }
}

impl AcpSessionDefaultsConfig {
    fn default_with_model(model: &ModelConfig) -> Self {
        Self {
            system_prompt: model.system_prompt.clone(),
            reasoning_effort: model.reasoning_effort.clone(),
        }
    }

    fn from_raw(raw: Option<RawAcpSessionDefaultsConfig>, model: &ModelConfig) -> Result<Self> {
        let defaults = Self::default_with_model(model);
        let Some(raw) = raw else {
            return Ok(defaults);
        };

        Ok(Self {
            system_prompt: raw.system_prompt.unwrap_or(defaults.system_prompt),
            reasoning_effort: raw
                .reasoning_effort
                .map(validate_model_reasoning_effort)
                .transpose()?
                .or(defaults.reasoning_effort),
        })
    }
}

impl LoggingConfig {
    pub fn default_with_data_dir(data_dir: &Path) -> Self {
        Self {
            file: LoggingFileConfig {
                enabled: true,
                path: data_dir.join("fluent-code.log"),
                level: "debug".to_string(),
            },
            stderr: LoggingStderrConfig {
                enabled: true,
                level: "warn".to_string(),
            },
        }
    }

    fn from_raw(raw: RawLoggingConfig, data_dir: &Path) -> Self {
        let default = Self::default_with_data_dir(data_dir);

        let file = raw.file.unwrap_or_default();
        let stderr = raw.stderr.unwrap_or_default();

        Self {
            file: LoggingFileConfig {
                enabled: file.enabled.unwrap_or(default.file.enabled),
                path: file
                    .path
                    .map(|path| resolve_path(data_dir, path))
                    .unwrap_or(default.file.path),
                level: file.level.unwrap_or(default.file.level),
            },
            stderr: LoggingStderrConfig {
                enabled: stderr.enabled.unwrap_or(default.stderr.enabled),
                level: stderr.level.unwrap_or(default.stderr.level),
            },
        }
    }
}

impl PluginConfig {
    fn default_with_paths(current_dir: &Path, data_dir: &Path) -> Self {
        Self {
            enable_project_plugins: true,
            enable_global_plugins: true,
            project_dir: current_dir.join(".fluent-code").join("plugins"),
            global_dir: data_dir.join("plugins"),
        }
    }

    fn from_raw(raw: RawPluginConfig, current_dir: &Path, data_dir: &Path) -> Self {
        let default = Self::default_with_paths(current_dir, data_dir);

        Self {
            enable_project_plugins: raw
                .enable_project_plugins
                .unwrap_or(default.enable_project_plugins),
            enable_global_plugins: raw
                .enable_global_plugins
                .unwrap_or(default.enable_global_plugins),
            project_dir: raw
                .project_dir
                .map(|path| resolve_path(current_dir, path))
                .unwrap_or(default.project_dir),
            global_dir: raw
                .global_dir
                .map(|path| resolve_path(data_dir, path))
                .unwrap_or(default.global_dir),
        }
    }
}

impl AgentConfig {
    fn from_raw(raw: RawAgentConfig) -> Self {
        Self {
            name: raw.name,
            description: raw.description,
            system_prompt: raw.system_prompt,
            tools_allowed: raw.tools_allowed,
            tools_denied: raw.tools_denied,
            delegation_targets: raw.delegation_targets,
        }
    }
}

fn resolve_path(current_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        current_dir.join(path)
    }
}

fn validate_model_reasoning_effort(value: String) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();

    if is_supported_reasoning_effort(&normalized) {
        return Ok(normalized);
    }

    Err(FluentCodeError::Config(format!(
        "invalid model_reasoning_effort value '{value}'; expected one of: none, minimal, low, medium, high, xhigh"
    )))
}

fn validate_acp_protocol_version(value: u16) -> Result<u16> {
    if value == DEFAULT_ACP_PROTOCOL_VERSION {
        return Ok(value);
    }

    Err(FluentCodeError::Config(format!(
        "unsupported acp.protocol_version value '{value}'; expected {DEFAULT_ACP_PROTOCOL_VERSION}"
    )))
}

fn is_supported_reasoning_effort(value: &str) -> bool {
    matches!(
        value,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use fluent_code_provider::WireApi;

    use super::{
        AcpAuthMethodConfig, AgentConfig, Config, LoggingConfig, validate_acp_protocol_version,
    };
    use crate::FluentCodeError;

    #[test]
    fn parses_toml_model_and_provider_settings() {
        let config = Config::from_toml_str(
            r#"
data_dir = ".fluent-code-data"
model_provider = "openai"
model = "gpt-5.4"
model_reasoning_effort = "medium"
system_prompt = "You are a precise coding assistant."

[model_providers.openai]
base_url = "https://example.com/v1"
wire_api = "responses"
api_keys = ["sk-live-primary"]
api_key_envs = ["OPENAI_API_KEY", "OPENAI_FALLBACK_KEY"]
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse toml config");

        assert_eq!(
            config.data_dir,
            Path::new("/tmp/fluent-code-config/.fluent-code-data")
        );
        assert_eq!(
            config.logging,
            LoggingConfig::default_with_data_dir(Path::new(
                "/tmp/fluent-code-config/.fluent-code-data"
            ))
        );
        assert_eq!(
            config.plugins.project_dir,
            Path::new("/tmp/fluent-code-config/.fluent-code/plugins")
        );
        assert_eq!(
            config.plugins.global_dir,
            Path::new("/tmp/fluent-code-config/.fluent-code-data/plugins")
        );
        assert_eq!(config.model.provider, "openai");
        assert_eq!(config.model.model, "gpt-5.4");
        assert_eq!(config.model.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(
            config.model.system_prompt,
            "You are a precise coding assistant."
        );

        let openai = config
            .model_providers
            .get("openai")
            .expect("openai provider config");
        assert_eq!(openai.base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(openai.wire_api, Some(WireApi::Responses));
        assert_eq!(openai.api_keys, vec!["sk-live-primary"]);
        assert_eq!(
            openai.api_key_envs,
            vec!["OPENAI_API_KEY", "OPENAI_FALLBACK_KEY"]
        );
    }

    #[test]
    fn defaults_when_toml_is_empty() {
        let config = Config::from_toml_str("", Path::new("/tmp/fluent-code-config"))
            .expect("parse empty config");

        assert_eq!(config.model.provider, "mock");
        assert_eq!(config.model.model, "gpt-4.1-mini");
        assert!(config.model.reasoning_effort.is_none());
        assert_eq!(
            config.logging,
            LoggingConfig::default_with_data_dir(Path::new("/tmp/fluent-code-config/.fluent-code"))
        );
        assert!(config.plugins.enable_project_plugins);
        assert!(config.plugins.enable_global_plugins);
        assert_eq!(
            config.plugins.project_dir,
            Path::new("/tmp/fluent-code-config/.fluent-code/plugins")
        );
        assert_eq!(
            config.plugins.global_dir,
            Path::new("/tmp/fluent-code-config/.fluent-code/plugins")
        );
        assert_eq!(
            config.model.system_prompt,
            "You are a helpful coding assistant."
        );
        assert_eq!(config.acp.protocol_version, 1);
        assert!(config.acp.auth_methods.is_empty());
        assert_eq!(
            config.acp.session_defaults.system_prompt,
            "You are a helpful coding assistant."
        );
        assert!(config.acp.session_defaults.reasoning_effort.is_none());
        assert!(config.agents.is_none());
        assert!(config.model_providers.is_empty());
    }

    #[test]
    fn acp_defaults_follow_model_settings_when_not_overridden() {
        let config = Config::from_toml_str(
            r#"
model_reasoning_effort = "medium"
system_prompt = "Model default prompt"
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse config with model defaults");

        assert_eq!(
            config.acp.session_defaults.system_prompt,
            "Model default prompt"
        );
        assert_eq!(
            config.acp.session_defaults.reasoning_effort.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn parses_acp_config_settings() {
        let config = Config::from_toml_str(
            r#"
[acp]
protocol_version = 1

[[acp.auth_methods]]
id = "api_key"
name = "API key"
description = "Provide a bearer token."

[acp.session_defaults]
system_prompt = "ACP session prompt"
reasoning_effort = "high"
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse acp config");

        assert_eq!(config.acp.protocol_version, 1);
        assert_eq!(
            config.acp.auth_methods,
            vec![AcpAuthMethodConfig {
                id: "api_key".to_string(),
                name: "API key".to_string(),
                description: Some("Provide a bearer token.".to_string()),
            }]
        );
        assert_eq!(
            config.acp.session_defaults.system_prompt,
            "ACP session prompt"
        );
        assert_eq!(
            config.acp.session_defaults.reasoning_effort.as_deref(),
            Some("high")
        );
    }

    #[test]
    fn rejects_invalid_model_reasoning_effort() {
        let error = Config::from_toml_str(
            r#"
model_reasoning_effort = "turbo"
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect_err("invalid reasoning effort should fail config loading");

        assert!(matches!(
            error,
            FluentCodeError::Config(message)
                if message == "invalid model_reasoning_effort value 'turbo'; expected one of: none, minimal, low, medium, high, xhigh"
        ));
    }

    #[test]
    fn normalizes_model_reasoning_effort_during_loading() {
        let config = Config::from_toml_str(
            r#"
model_reasoning_effort = " High "
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("supported reasoning effort should parse after normalization");

        assert_eq!(config.model.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn rejects_unsupported_acp_protocol_version() {
        let error = Config::from_toml_str(
            r#"
[acp]
protocol_version = 99
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect_err("unsupported ACP protocol version should fail");

        assert!(matches!(
            error,
            FluentCodeError::Config(message)
                if message == "unsupported acp.protocol_version value '99'; expected 1"
        ));
    }

    #[test]
    fn rejects_unknown_acp_config_fields() {
        let error = Config::from_toml_str(
            r#"
[acp]
session_list = true
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect_err("unsupported ACP config field should fail");

        assert!(matches!(error, FluentCodeError::Toml(_)));
    }

    #[test]
    fn validate_acp_protocol_version_accepts_supported_version() {
        assert_eq!(validate_acp_protocol_version(1).unwrap(), 1);
    }

    #[test]
    fn parses_agent_config_entries() {
        let config = Config::from_toml_str(
            r#"
[[agents]]
name = "oracle"
description = "Answer architecture questions."
system_prompt = "You are the oracle subagent."

[[agents]]
name = "reviewer"
description = "Review code changes."
system_prompt = "You are the reviewer subagent."
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse agent config");

        assert_eq!(
            config.agents,
            Some(vec![
                AgentConfig {
                    name: "oracle".to_string(),
                    description: "Answer architecture questions.".to_string(),
                    system_prompt: "You are the oracle subagent.".to_string(),
                    tools_allowed: None,
                    tools_denied: None,
                    delegation_targets: None,
                },
                AgentConfig {
                    name: "reviewer".to_string(),
                    description: "Review code changes.".to_string(),
                    system_prompt: "You are the reviewer subagent.".to_string(),
                    tools_allowed: None,
                    tools_denied: None,
                    delegation_targets: None,
                },
            ])
        );
    }

    #[test]
    fn parses_logging_config_and_resolves_relative_file_path_under_data_dir() {
        let config = Config::from_toml_str(
            r#"
data_dir = ".runtime"

[logging.file]
enabled = true
path = "logs/internal.jsonl"
level = "trace"

[logging.stderr]
enabled = false
level = "error"
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse logging config");

        assert_eq!(
            config.data_dir,
            Path::new("/tmp/fluent-code-config/.runtime")
        );
        assert!(config.logging.file.enabled);
        assert_eq!(config.logging.file.level, "trace");
        assert_eq!(
            config.logging.file.path,
            Path::new("/tmp/fluent-code-config/.runtime/logs/internal.jsonl")
        );
        assert!(!config.logging.stderr.enabled);
        assert_eq!(config.logging.stderr.level, "error");
    }

    #[test]
    fn preserves_absolute_logging_file_path() {
        let config = Config::from_toml_str(
            r#"
[logging.file]
path = "/var/tmp/fluent-code.log"
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse absolute log path");

        assert_eq!(
            config.logging.file.path,
            Path::new("/var/tmp/fluent-code.log")
        );
    }

    #[test]
    fn parses_plugin_config_and_resolves_relative_paths() {
        let config = Config::from_toml_str(
            r#"
data_dir = ".runtime"

[plugins]
enable_project_plugins = false
enable_global_plugins = true
project_dir = ".custom-project-plugins"
global_dir = "plugin-cache"
"#,
            Path::new("/tmp/fluent-code-config"),
        )
        .expect("parse plugin config");

        assert!(!config.plugins.enable_project_plugins);
        assert!(config.plugins.enable_global_plugins);
        assert_eq!(
            config.plugins.project_dir,
            Path::new("/tmp/fluent-code-config/.custom-project-plugins")
        );
        assert_eq!(
            config.plugins.global_dir,
            Path::new("/tmp/fluent-code-config/.runtime/plugin-cache")
        );
    }
}
