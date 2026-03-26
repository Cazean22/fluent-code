mod discovery;
mod host;
mod manifest;
mod registry;

use tracing::warn;

use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::error::Result;

pub use discovery::DiscoveryScope;
pub use registry::{ToolPolicy, ToolPolicyOrigin, ToolRegistry};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginLoadSnapshot {
    pub accepted_plugins: Vec<LoadedPluginMetadata>,
    pub warnings: Vec<String>,
}

impl PluginLoadSnapshot {
    pub fn plugin_count(&self) -> usize {
        self.accepted_plugins.len()
    }

    pub fn warning_count(&self) -> usize {
        self.warnings.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPluginMetadata {
    pub name: String,
    pub id: String,
    pub version: String,
    pub scope: DiscoveryScope,
    pub description: Option<String>,
    pub tool_names: Vec<String>,
    pub tool_count: usize,
}

impl LoadedPluginMetadata {
    pub fn tool_count(&self) -> usize {
        self.tool_count
    }
}

#[derive(Debug, Clone)]
pub struct LoadedToolRegistry {
    pub tool_registry: ToolRegistry,
    pub plugin_load_snapshot: PluginLoadSnapshot,
}

pub fn load_tool_registry(config: &Config) -> Result<LoadedToolRegistry> {
    let discovery = discovery::discover_plugins(&config.plugins);
    let agent_registry = AgentRegistry::from_configured(config.agents.as_deref())?;
    build_loaded_tool_registry(&agent_registry, discovery)
}

fn build_loaded_tool_registry(
    agent_registry: &AgentRegistry,
    discovery: discovery::PluginDiscovery,
) -> Result<LoadedToolRegistry> {
    let mut accepted_plugins = Vec::new();
    let mut accepted_plugin_metadata = Vec::new();
    let mut warnings = Vec::new();

    for warning_message in discovery.warnings {
        warn!(warning = %warning_message, "plugin discovery warning");
        warnings.push(warning_message);
    }

    for plugin in discovery.plugins {
        let mut candidate_plugins = accepted_plugins.clone();
        candidate_plugins.push(plugin.clone());

        match validate_candidate_plugins(agent_registry, candidate_plugins) {
            Ok(_) => {
                accepted_plugin_metadata.push(LoadedPluginMetadata::from_discovered(&plugin));
                accepted_plugins.push(plugin);
            }
            Err(error) => {
                let warning_message = format!(
                    "plugin '{}' disabled during registry validation: {error}",
                    plugin.manifest.id
                );
                warn!(warning = %warning_message, "plugin disabled during registry validation");
                warnings.push(warning_message);
            }
        }
    }

    Ok(LoadedToolRegistry {
        tool_registry: build_tool_registry(agent_registry, accepted_plugins)?,
        plugin_load_snapshot: PluginLoadSnapshot {
            accepted_plugins: accepted_plugin_metadata,
            warnings,
        },
    })
}

impl LoadedPluginMetadata {
    fn from_discovered(plugin: &discovery::DiscoveredPlugin) -> Self {
        let tool_names = plugin
            .manifest
            .tools
            .iter()
            .filter(|tool| tool.enabled)
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();

        Self {
            name: plugin.manifest.name.clone(),
            id: plugin.manifest.id.clone(),
            version: plugin.manifest.version.clone(),
            scope: plugin.scope,
            description: (!plugin.manifest.description.trim().is_empty())
                .then(|| plugin.manifest.description.clone()),
            tool_count: tool_names.len(),
            tool_names,
        }
    }
}

fn validate_candidate_plugins(
    agent_registry: &AgentRegistry,
    candidate_plugins: Vec<discovery::DiscoveredPlugin>,
) -> Result<()> {
    #[cfg(test)]
    {
        registry::ToolRegistry::from_discovered_with_noop_executor(
            agent_registry,
            candidate_plugins,
        )
        .map(|_| ())
    }

    #[cfg(not(test))]
    registry::ToolRegistry::from_discovered(agent_registry, candidate_plugins).map(|_| ())
}

fn build_tool_registry(
    agent_registry: &AgentRegistry,
    candidate_plugins: Vec<discovery::DiscoveredPlugin>,
) -> Result<ToolRegistry> {
    #[cfg(test)]
    {
        registry::ToolRegistry::from_discovered_with_noop_executor(
            agent_registry,
            candidate_plugins,
        )
    }

    #[cfg(not(test))]
    {
        registry::ToolRegistry::from_discovered(agent_registry, candidate_plugins)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::agent::AgentRegistry;
    use crate::config::{Config, LoggingConfig, ModelConfig, PluginConfig};
    use crate::plugin::discovery::{DiscoveredPlugin, PluginDiscovery};
    use crate::plugin::manifest::{
        FilesystemCapability, PluginCapabilities, PluginHostRequirements, PluginManifest,
        PluginRuntime, PluginToolManifest, TOOL_PLUGIN_API_VERSION,
    };

    use super::{build_loaded_tool_registry, load_tool_registry};

    #[test]
    fn load_tool_registry_preserves_plugin_snapshot_and_warnings() {
        let loaded = build_loaded_tool_registry(
            AgentRegistry::built_in(),
            PluginDiscovery {
                plugins: vec![
                    discovered_plugin(
                        "global.docs",
                        "Docs Plugin",
                        "0.2.0",
                        super::DiscoveryScope::Global,
                        Some("Indexes docs for the workspace."),
                        &["docs_search", "docs_read"],
                    ),
                    discovered_plugin(
                        "project.docs",
                        "Project Docs",
                        "1.0.0",
                        super::DiscoveryScope::Project,
                        None,
                        &["read"],
                    ),
                ],
                warnings: vec![
                    "failed to parse plugin manifest '/tmp/broken/plugin.toml': invalid type"
                        .to_string(),
                ],
            },
        )
        .expect("load plugin registry with snapshot");

        assert_eq!(loaded.plugin_load_snapshot.plugin_count(), 1);
        assert_eq!(loaded.plugin_load_snapshot.warning_count(), 2);
        assert_eq!(loaded.tool_registry.provider_tools().len(), 7);

        let plugin = &loaded.plugin_load_snapshot.accepted_plugins[0];
        assert_eq!(plugin.name, "Docs Plugin");
        assert_eq!(plugin.id, "global.docs");
        assert_eq!(plugin.version, "0.2.0");
        assert_eq!(plugin.scope, super::DiscoveryScope::Global);
        assert_eq!(
            plugin.description.as_deref(),
            Some("Indexes docs for the workspace.")
        );
        assert_eq!(plugin.tool_names, vec!["docs_search", "docs_read"]);
        assert_eq!(plugin.tool_count(), 2);

        assert!(
            loaded
                .plugin_load_snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("failed to parse plugin manifest"))
        );
        assert!(
            loaded
                .plugin_load_snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("disabled during registry validation"))
        );
    }

    #[test]
    fn load_tool_registry_returns_snapshot_alongside_registry() {
        let base = unique_test_dir();
        let global_dir = base.join("global");

        write_plugin(
            &global_dir.join("global-docs"),
            "global.docs",
            "Docs Plugin",
            "0.2.0",
            Some("Indexes docs for the workspace."),
            &["docs_search", "docs_read"],
        );
        let project_dir = base.join("project");
        let config = test_config(project_dir, global_dir, base.join("data"));
        let loaded = load_tool_registry(&config).expect("load plugin registry with snapshot");

        assert_eq!(loaded.plugin_load_snapshot.plugin_count(), 1);
        assert_eq!(loaded.plugin_load_snapshot.warning_count(), 0);
        assert_eq!(loaded.tool_registry.provider_tools().len(), 7);

        cleanup(&base);
    }

    fn test_config(
        project_dir: std::path::PathBuf,
        global_dir: std::path::PathBuf,
        data_dir: std::path::PathBuf,
    ) -> Config {
        Config {
            config_path: None,
            data_dir: data_dir.clone(),
            logging: LoggingConfig::default_with_data_dir(&data_dir),
            model: ModelConfig {
                provider: "mock".to_string(),
                model: "test-model".to_string(),
                reasoning_effort: None,
                system_prompt: "test".to_string(),
            },
            agents: None,
            plugins: PluginConfig {
                enable_project_plugins: true,
                enable_global_plugins: true,
                project_dir,
                global_dir,
            },
            model_providers: std::collections::HashMap::new(),
        }
    }

    fn write_plugin(
        plugin_dir: &std::path::Path,
        plugin_id: &str,
        plugin_name: &str,
        version: &str,
        description: Option<&str>,
        tool_names: &[&str],
    ) {
        fs::create_dir_all(plugin_dir).expect("create plugin dir");

        let mut manifest = format!(
            "\n[plugin]\nid = \"{plugin_id}\"\nname = \"{plugin_name}\"\nversion = \"{version}\"\napi_version = \"{}\"\n",
            crate::plugin::manifest::TOOL_PLUGIN_API_VERSION
        );

        if let Some(description) = description {
            manifest.push_str(&format!("description = \"{description}\"\n"));
        }

        manifest.push_str("\n[component]\npath = \"plugin.wasm\"\n");

        for tool_name in tool_names {
            manifest.push_str(&format!(
                "\n[[tools]]\nname = \"{tool_name}\"\ninput_schema = {{ type = \"object\" }}\n"
            ));
        }

        fs::write(plugin_dir.join("plugin.toml"), manifest).expect("write plugin manifest");
        fs::write(plugin_dir.join("plugin.wasm"), b"test-component")
            .expect("write placeholder plugin component");
    }

    fn discovered_plugin(
        id: &str,
        name: &str,
        version: &str,
        scope: super::DiscoveryScope,
        description: Option<&str>,
        tool_names: &[&str],
    ) -> DiscoveredPlugin {
        DiscoveredPlugin {
            scope,
            plugin_dir: PathBuf::from(format!("/tmp/{id}")),
            manifest: PluginManifest {
                manifest_version: 1,
                id: id.to_string(),
                name: name.to_string(),
                version: version.to_string(),
                api_version: TOOL_PLUGIN_API_VERSION.to_string(),
                description: description.unwrap_or_default().to_string(),
                license: None,
                homepage: None,
                component_path: PathBuf::from(format!("/tmp/{id}/plugin.wasm")),
                component_sha256: None,
                runtime: PluginRuntime {
                    abi: "wasm-component".to_string(),
                    wit_world: "plugin".to_string(),
                    wasi: "p2".to_string(),
                },
                capabilities: PluginCapabilities {
                    filesystem: FilesystemCapability::None,
                    network: false,
                    process: false,
                    environment: Vec::new(),
                },
                host: PluginHostRequirements {
                    min_fluent_code_version: None,
                    max_fluent_code_version: None,
                    platforms: Vec::new(),
                },
                tools: tool_names
                    .iter()
                    .map(|tool_name| PluginToolManifest {
                        name: (*tool_name).to_string(),
                        description: format!("{tool_name} description"),
                        input_schema: serde_json::json!({ "type": "object" }),
                        enabled: true,
                        requires_approval: true,
                        timeout_ms: 30_000,
                        max_output_bytes: 65_536,
                        capabilities: PluginCapabilities {
                            filesystem: FilesystemCapability::None,
                            network: false,
                            process: false,
                            environment: Vec::new(),
                        },
                    })
                    .collect(),
            },
        }
    }

    fn unique_test_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();

        std::env::temp_dir().join(format!("fluent-code-plugin-load-test-{nanos}"))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = fs::remove_dir_all(path);
    }
}
