use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use fluent_code_provider::{ProviderTool, ProviderToolCall};

use crate::agent::AgentRegistry;
use crate::error::{FluentCodeError, Result};
use crate::plugin::discovery::{DiscoveredPlugin, DiscoveryScope};
use crate::plugin::manifest::PluginCapabilities;
use crate::session::model::{ToolPermissionAction, ToolSource};
use crate::tool::{built_in_tool_names, built_in_tools, execute_built_in_tool};

use super::host::WasmPluginExecutor;

struct NoopPluginExecutor;

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    provider_tools: Vec<ProviderTool>,
    plugin_executor: Arc<dyn PluginExecutor>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tool_count", &self.tools.len())
            .field("provider_tool_count", &self.provider_tools.len())
            .finish()
    }
}

#[derive(Debug, Clone)]
enum RegisteredTool {
    BuiltIn,
    Plugin(PluginToolRegistration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginToolRegistration {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub tool_name: String,
    pub scope: DiscoveryScope,
    pub component_path: PathBuf,
    pub requires_approval: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub capabilities: PluginCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicyOrigin {
    BuiltInDefault,
    PluginManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    pub tool_name: String,
    pub tool_source: ToolSource,
    pub default_action: ToolPermissionAction,
    pub rememberable: bool,
    pub origin: ToolPolicyOrigin,
}

impl PluginToolRegistration {
    fn tool_source(&self) -> ToolSource {
        ToolSource::Plugin {
            plugin_id: self.plugin_id.clone(),
            plugin_name: self.plugin_name.clone(),
            plugin_version: self.plugin_version.clone(),
            scope: self.scope,
        }
    }
}

trait PluginExecutor: Send + Sync {
    fn validate_manifest(&self, manifest: &crate::plugin::manifest::PluginManifest) -> Result<()>;

    fn execute(
        &self,
        registration: &PluginToolRegistration,
        tool_call: &ProviderToolCall,
    ) -> Result<String>;
}

impl PluginExecutor for NoopPluginExecutor {
    fn validate_manifest(&self, _manifest: &crate::plugin::manifest::PluginManifest) -> Result<()> {
        Ok(())
    }

    fn execute(
        &self,
        registration: &PluginToolRegistration,
        _tool_call: &ProviderToolCall,
    ) -> Result<String> {
        Err(FluentCodeError::Plugin(format!(
            "plugin '{}' is not executable in the default built-in registry",
            registration.plugin_id
        )))
    }
}

impl PluginExecutor for WasmPluginExecutor {
    fn validate_manifest(&self, manifest: &crate::plugin::manifest::PluginManifest) -> Result<()> {
        self.validate_manifest(manifest)
    }

    fn execute(
        &self,
        registration: &PluginToolRegistration,
        tool_call: &ProviderToolCall,
    ) -> Result<String> {
        let input_json = serde_json::to_string(&tool_call.arguments)?;
        self.execute(registration, &input_json)
    }
}

impl ToolRegistry {
    pub fn built_in() -> Self {
        Self::with_agent_registry(AgentRegistry::built_in())
    }

    pub fn with_agent_registry(agent_registry: &AgentRegistry) -> Self {
        Self::from_discovered_with_executor(
            agent_registry,
            Vec::new(),
            Arc::new(NoopPluginExecutor),
        )
        .expect("built-in tool registry should always construct")
    }

    pub fn from_discovered(
        agent_registry: &AgentRegistry,
        plugins: Vec<DiscoveredPlugin>,
    ) -> Result<Self> {
        Self::from_discovered_with_executor(
            agent_registry,
            plugins,
            Arc::new(WasmPluginExecutor::new()?),
        )
    }

    #[cfg(test)]
    pub(crate) fn from_discovered_with_noop_executor(
        agent_registry: &AgentRegistry,
        plugins: Vec<DiscoveredPlugin>,
    ) -> Result<Self> {
        Self::from_discovered_with_executor(agent_registry, plugins, Arc::new(NoopPluginExecutor))
    }

    #[cfg(test)]
    pub(crate) fn with_plugin_tool_source_for_tests(
        tool_name: &str,
        plugin_id: &str,
        plugin_name: &str,
        plugin_version: &str,
        scope: DiscoveryScope,
    ) -> Self {
        let mut registry = Self::built_in();
        registry.tools.insert(
            tool_name.to_string(),
            RegisteredTool::Plugin(PluginToolRegistration {
                plugin_id: plugin_id.to_string(),
                plugin_name: plugin_name.to_string(),
                plugin_version: plugin_version.to_string(),
                tool_name: tool_name.to_string(),
                scope,
                component_path: PathBuf::from("/tmp/test-plugin.wasm"),
                requires_approval: true,
                timeout_ms: 30_000,
                max_output_bytes: 65_536,
                capabilities: PluginCapabilities {
                    filesystem: crate::plugin::manifest::FilesystemCapability::None,
                    network: false,
                    process: false,
                    environment: Vec::new(),
                },
            }),
        );
        registry
    }

    fn from_discovered_with_executor(
        agent_registry: &AgentRegistry,
        plugins: Vec<DiscoveredPlugin>,
        plugin_executor: Arc<dyn PluginExecutor>,
    ) -> Result<Self> {
        let mut tools = HashMap::new();
        let mut provider_tools = built_in_tools(agent_registry);
        let mut plugin_ids_by_scope = HashMap::<(String, DiscoveryScope), String>::new();

        for tool in &provider_tools {
            tools.insert(tool.name.clone(), RegisteredTool::BuiltIn);
        }

        for plugin in plugins {
            if let Some(existing_plugin_name) = plugin_ids_by_scope.insert(
                (plugin.manifest.id.clone(), plugin.scope),
                plugin.manifest.name.clone(),
            ) {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' collides with plugin '{}' for plugin id '{}' in scope {:?}",
                    plugin.manifest.name, existing_plugin_name, plugin.manifest.id, plugin.scope
                )));
            }

            plugin_executor.validate_manifest(&plugin.manifest)?;

            for plugin_tool in plugin.manifest.tools {
                if built_in_tool_names().contains(&plugin_tool.name.as_str()) {
                    return Err(FluentCodeError::Plugin(format!(
                        "plugin '{}' declares reserved built-in tool name '{}'",
                        plugin.manifest.id, plugin_tool.name
                    )));
                }

                let registration = PluginToolRegistration {
                    plugin_id: plugin.manifest.id.clone(),
                    plugin_name: plugin.manifest.name.clone(),
                    plugin_version: plugin.manifest.version.clone(),
                    tool_name: plugin_tool.name.clone(),
                    scope: plugin.scope,
                    component_path: plugin.manifest.component_path.clone(),
                    requires_approval: plugin_tool.requires_approval,
                    timeout_ms: plugin_tool.timeout_ms,
                    max_output_bytes: plugin_tool.max_output_bytes,
                    capabilities: plugin_tool.capabilities.clone(),
                };

                if !plugin_tool.enabled {
                    continue;
                }

                let provider_tool = ProviderTool {
                    name: plugin_tool.name.clone(),
                    description: plugin_tool.description,
                    input_schema: plugin_tool.input_schema,
                };

                match tools.get(&provider_tool.name) {
                    Some(RegisteredTool::BuiltIn) => {
                        return Err(FluentCodeError::Plugin(format!(
                            "plugin '{}' declares reserved built-in tool name '{}'",
                            registration.plugin_id, provider_tool.name
                        )));
                    }
                    Some(RegisteredTool::Plugin(existing))
                        if existing.scope == registration.scope =>
                    {
                        return Err(FluentCodeError::Plugin(format!(
                            "plugin '{}' collides with plugin '{}' for tool '{}' in scope {:?}",
                            registration.plugin_id,
                            existing.plugin_id,
                            provider_tool.name,
                            registration.scope
                        )));
                    }
                    _ => {}
                }

                tools.insert(
                    provider_tool.name.clone(),
                    RegisteredTool::Plugin(registration),
                );
                provider_tools.retain(|tool| tool.name != provider_tool.name);
                provider_tools.push(provider_tool);
            }
        }

        provider_tools.sort_by(|left, right| left.name.cmp(&right.name));

        Ok(Self {
            tools,
            provider_tools,
            plugin_executor,
        })
    }

    pub fn provider_tools(&self) -> Vec<ProviderTool> {
        self.provider_tools.clone()
    }

    /// Return the subset of provider tools that `agent_permissions` permits.
    pub fn provider_tools_for_agent(
        &self,
        agent_permissions: &crate::agent::AgentToolPermissions,
    ) -> Vec<ProviderTool> {
        self.provider_tools
            .iter()
            .filter(|tool| agent_permissions.is_tool_permitted(&tool.name))
            .cloned()
            .collect()
    }

    pub fn execute(&self, tool_call: &ProviderToolCall) -> Result<String> {
        match self.tools.get(&tool_call.name) {
            Some(RegisteredTool::BuiltIn) => execute_built_in_tool(tool_call),
            Some(RegisteredTool::Plugin(registration)) => {
                self.plugin_executor.execute(registration, tool_call)
            }
            None => Err(FluentCodeError::Provider(format!(
                "unsupported tool '{}'",
                tool_call.name
            ))),
        }
    }

    pub fn tool_source(&self, tool_name: &str) -> ToolSource {
        match self.tools.get(tool_name) {
            Some(RegisteredTool::BuiltIn) | None => ToolSource::BuiltIn,
            Some(RegisteredTool::Plugin(registration)) => registration.tool_source(),
        }
    }

    pub fn plugin_registration(&self, tool_name: &str) -> Option<&PluginToolRegistration> {
        match self.tools.get(tool_name) {
            Some(RegisteredTool::Plugin(registration)) => Some(registration),
            _ => None,
        }
    }

    pub fn tool_policy(&self, tool_name: &str) -> Option<ToolPolicy> {
        match self.tools.get(tool_name) {
            Some(RegisteredTool::BuiltIn) => Some(built_in_tool_policy(tool_name)),
            Some(RegisteredTool::Plugin(registration)) => Some(ToolPolicy {
                tool_name: tool_name.to_string(),
                tool_source: registration.tool_source(),
                default_action: if registration.requires_approval {
                    ToolPermissionAction::Ask
                } else {
                    ToolPermissionAction::Allow
                },
                rememberable: true,
                origin: ToolPolicyOrigin::PluginManifest,
            }),
            None => None,
        }
    }
}

fn built_in_tool_policy(tool_name: &str) -> ToolPolicy {
    let (default_action, rememberable) = match tool_name {
        "task" => (ToolPermissionAction::Ask, false),
        "uppercase_text" => (ToolPermissionAction::Ask, true),
        "read" | "glob" | "grep" => (ToolPermissionAction::Ask, true),
        _ => (ToolPermissionAction::Ask, true),
    };

    ToolPolicy {
        tool_name: tool_name.to_string(),
        tool_source: ToolSource::BuiltIn,
        default_action,
        rememberable,
        origin: ToolPolicyOrigin::BuiltInDefault,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use fluent_code_provider::ProviderToolCall;
    use serde_json::json;

    use super::{PluginExecutor, ToolRegistry};
    use crate::agent::AgentRegistry;
    use crate::plugin::discovery::{DiscoveredPlugin, DiscoveryScope};
    use crate::plugin::manifest::{
        FilesystemCapability, PluginCapabilities, PluginHostRequirements, PluginManifest,
        PluginRuntime, PluginToolManifest, TOOL_PLUGIN_API_VERSION,
    };

    #[derive(Default)]
    struct FakePluginExecutor {
        outputs: Mutex<VecDeque<crate::error::Result<String>>>,
    }

    impl FakePluginExecutor {
        fn with_output(output: crate::error::Result<String>) -> Self {
            let mut outputs = VecDeque::new();
            outputs.push_back(output);
            Self {
                outputs: Mutex::new(outputs),
            }
        }
    }

    impl PluginExecutor for FakePluginExecutor {
        fn validate_manifest(
            &self,
            _manifest: &crate::plugin::manifest::PluginManifest,
        ) -> crate::error::Result<()> {
            Ok(())
        }

        fn execute(
            &self,
            _registration: &super::PluginToolRegistration,
            _tool_call: &ProviderToolCall,
        ) -> crate::error::Result<String> {
            self.outputs
                .lock()
                .expect("lock fake executor outputs")
                .pop_front()
                .expect("queued fake executor output")
        }
    }

    #[test]
    fn project_plugin_overrides_global_plugin_with_same_name() {
        let registry = ToolRegistry::from_discovered_with_executor(
            AgentRegistry::built_in(),
            vec![
                plugin("global.echo", DiscoveryScope::Global, "plugin_echo"),
                plugin("project.echo", DiscoveryScope::Project, "plugin_echo"),
            ],
            Arc::new(FakePluginExecutor::with_output(Ok("ok".to_string()))),
        )
        .expect("build registry");

        let tools = registry.provider_tools();
        assert_eq!(
            tools
                .iter()
                .filter(|tool| tool.name == "plugin_echo")
                .count(),
            1
        );
    }

    #[test]
    fn built_in_tool_names_are_reserved() {
        let error = ToolRegistry::from_discovered_with_executor(
            AgentRegistry::built_in(),
            vec![plugin("project.echo", DiscoveryScope::Project, "read")],
            Arc::new(FakePluginExecutor::default()),
        )
        .expect_err("reserved built-in names should fail");

        assert!(error.to_string().contains("reserved built-in tool name"));
    }

    #[test]
    fn plugin_tools_execute_through_plugin_executor() {
        let registry = ToolRegistry::from_discovered_with_executor(
            AgentRegistry::built_in(),
            vec![plugin(
                "project.echo",
                DiscoveryScope::Project,
                "plugin_echo",
            )],
            Arc::new(FakePluginExecutor::with_output(Ok(
                "plugin result".to_string()
            ))),
        )
        .expect("build registry");

        let result = registry.execute(&ProviderToolCall {
            id: "call-1".to_string(),
            name: "plugin_echo".to_string(),
            arguments: json!({ "text": "hello" }),
        });

        assert_eq!(result.expect("plugin tool result"), "plugin result");
    }

    #[test]
    fn task_tool_schema_uses_configured_agents() {
        let agent_registry = AgentRegistry::from_agent_configs(&[crate::config::AgentConfig {
            name: "oracle".to_string(),
            description: "Answer architecture questions.".to_string(),
            system_prompt: "You are the oracle subagent.".to_string(),
            tools_allowed: None,
            tools_denied: None,
            delegation_targets: None,
        }])
        .expect("custom agent registry");

        let registry = ToolRegistry::with_agent_registry(&agent_registry);
        let task_tool = registry
            .provider_tools()
            .into_iter()
            .find(|tool| tool.name == "task")
            .expect("task tool in provider registry");

        assert_eq!(
            task_tool.input_schema["properties"]["agent"]["enum"],
            json!(["oracle"])
        );
        assert!(task_tool.description.contains("oracle"));
        assert!(!task_tool.description.contains("explore"));
    }

    fn plugin(id: &str, scope: DiscoveryScope, tool_name: &str) -> DiscoveredPlugin {
        DiscoveredPlugin {
            scope,
            plugin_dir: PathBuf::from(format!("/tmp/{id}")),
            manifest: PluginManifest {
                manifest_version: 1,
                id: id.to_string(),
                name: id.to_string(),
                version: "0.1.0".to_string(),
                api_version: TOOL_PLUGIN_API_VERSION.to_string(),
                description: String::new(),
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
                tools: vec![PluginToolManifest {
                    name: tool_name.to_string(),
                    description: "plugin tool".to_string(),
                    input_schema: json!({ "type": "object" }),
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
                }],
            },
        }
    }
}
