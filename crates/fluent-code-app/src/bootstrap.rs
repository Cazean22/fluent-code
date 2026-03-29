use std::sync::Arc;

use fluent_code_provider::ProviderClient;

use crate::Result;
use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::logging::{LoggingGuard, init_logging};
use crate::plugin::{PluginLoadSnapshot, ToolRegistry, load_tool_registry};
use crate::runtime::Runtime;
use crate::session::store::FsSessionStore;

pub struct BootstrapContext {
    pub config: Config,
    pub store: FsSessionStore,
    pub agent_registry: Arc<AgentRegistry>,
    pub runtime: Runtime,
    pub tool_registry: Arc<ToolRegistry>,
    pub plugin_load_snapshot: PluginLoadSnapshot,
}

impl BootstrapContext {
    pub fn from_config(config: Config) -> Result<Self> {
        let store = FsSessionStore::new(config.data_dir.clone());
        let agent_registry = Arc::new(AgentRegistry::from_configured(config.agents.as_deref())?);
        let provider = ProviderClient::new(
            &config.model.provider,
            config.model.model.clone(),
            config.model.system_prompt.clone(),
            config.model.reasoning_effort.clone(),
            config.selected_provider_config().cloned(),
        )?;
        let loaded_tool_registry = load_tool_registry(&config)?;
        let tool_registry = Arc::new(loaded_tool_registry.tool_registry);
        let runtime = Runtime::new_with_tool_registry(provider, Arc::clone(&tool_registry));

        Ok(Self {
            config,
            store,
            agent_registry,
            runtime,
            tool_registry,
            plugin_load_snapshot: loaded_tool_registry.plugin_load_snapshot,
        })
    }
}

pub struct AppBootstrap {
    context: BootstrapContext,
    logging_guard: LoggingGuard,
}

impl AppBootstrap {
    pub fn load() -> Result<Self> {
        Self::from_config(Config::load()?)
    }

    pub fn from_config(config: Config) -> Result<Self> {
        let logging_guard = init_logging(&config)?;
        let context = BootstrapContext::from_config(config)?;
        Ok(Self {
            context,
            logging_guard,
        })
    }

    pub fn into_parts(self) -> (BootstrapContext, LoggingGuard) {
        (self.context, self.logging_guard)
    }
}
