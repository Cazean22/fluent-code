use std::collections::HashSet;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use fluent_code_provider::ProviderTool;

use crate::config::AgentConfig;
use crate::error::{FluentCodeError, Result};

pub const TASK_TOOL_NAME: &str = "task";

/// Structural role in the delegation graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentTier {
    Planner,
    Orchestrator,
    Specialist,
    #[default]
    Utility,
}

/// Functional classification of what the agent does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentCapability {
    Exploration,
    Research,
    Advisory,
    Implementation,
    Creative,
    Perception,
    Orchestration,
    #[default]
    Planning,
}

/// Economic governance hint for delegation decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentCostClass {
    Free,
    #[default]
    Cheap,
    Standard,
    Expensive,
}

/// Runtime context: who can invoke this agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    Primary,
    #[default]
    Subagent,
    Dual,
}

/// Per-agent tool permission ruleset. Controls which tools an agent may use.
///
/// Resolution order:
/// 1. If `tools_denied` contains the tool name or a wildcard match, **deny**.
/// 2. If `tools_allowed` is non-empty and does not contain the tool name or a
///    wildcard match, **deny**.
/// 3. Otherwise **allow**.
///
/// Wildcard patterns: `"*"` matches all tools, `"mcp(*)"` matches all MCP
/// tools, `"task(*)"` matches all task delegations. Exact names match only
/// themselves.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentToolPermissions {
    pub tools_allowed: Vec<String>,
    pub tools_denied: Vec<String>,
}

impl AgentToolPermissions {
    /// Returns `true` when the agent is permitted to invoke `tool_name`.
    pub fn is_tool_permitted(&self, tool_name: &str) -> bool {
        if pattern_list_matches(&self.tools_denied, tool_name) {
            return false;
        }
        if self.tools_allowed.is_empty() {
            return true;
        }
        pattern_list_matches(&self.tools_allowed, tool_name)
    }
}

/// Check whether any pattern in `patterns` matches `tool_name`.
fn pattern_list_matches(patterns: &[String], tool_name: &str) -> bool {
    patterns.iter().any(|pattern| tool_pattern_matches(pattern, tool_name))
}

/// Evaluate a single tool permission pattern against a tool name.
///
/// Supported patterns:
/// - `"*"` — matches everything
/// - `"all"` — matches everything
/// - `"task(*)"` — matches any tool starting with `task`
/// - `"mcp(*)"` — matches any tool starting with `mcp`
/// - exact name — literal equality
fn tool_pattern_matches(pattern: &str, tool_name: &str) -> bool {
    match pattern {
        "*" | "all" => true,
        p if p.ends_with("(*)") => {
            let prefix = &p[..p.len() - 3];
            tool_name == prefix || tool_name.starts_with(&format!("{prefix}_"))
                || tool_name.starts_with(&format!("{prefix}("))
        }
        p => p == tool_name,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub tier: AgentTier,
    pub capability: AgentCapability,
    pub cost_class: AgentCostClass,
    pub mode: AgentMode,
    pub temperature: Option<f32>,
    pub tool_permissions: AgentToolPermissions,
    /// Which agents this agent may delegate to via the task tool.
    pub delegation_targets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRegistry {
    agents: Vec<AgentDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRequest {
    pub agent: String,
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
struct TaskArguments {
    agent: String,
    prompt: String,
}

static BUILT_IN_REGISTRY: LazyLock<AgentRegistry> = LazyLock::new(|| {
    AgentRegistry {
        agents: vec![
            AgentDefinition {
                name: "explore".to_string(),
                description: "Investigate the repository, trace implementations, and report precise findings.".to_string(),
                system_prompt: "You are the explore subagent. Investigate the repository carefully, follow existing code patterns, and answer with concrete findings grounded in the code you read. Focus on discovery, not implementation.".to_string(),
                tier: AgentTier::Utility,
                capability: AgentCapability::Exploration,
                cost_class: AgentCostClass::Free,
                mode: AgentMode::Subagent,
                temperature: Some(0.1),
                tool_permissions: AgentToolPermissions {
                    tools_allowed: vec![
                        "read".to_string(),
                        "glob".to_string(),
                        "grep".to_string(),
                    ],
                    tools_denied: vec![
                        "task".to_string(),
                    ],
                },
                delegation_targets: vec![],
            },
            AgentDefinition {
                name: "librarian".to_string(),
                description: "Gather code-oriented reference material and summarize the most relevant details.".to_string(),
                system_prompt: "You are the librarian subagent. Read the available project context carefully and return a concise, well-organized reference summary that helps the parent agent continue implementation accurately.".to_string(),
                tier: AgentTier::Utility,
                capability: AgentCapability::Research,
                cost_class: AgentCostClass::Cheap,
                mode: AgentMode::Subagent,
                temperature: Some(0.1),
                tool_permissions: AgentToolPermissions {
                    tools_allowed: vec![
                        "read".to_string(),
                        "glob".to_string(),
                        "grep".to_string(),
                    ],
                    tools_denied: vec![
                        "task".to_string(),
                    ],
                },
                delegation_targets: vec![],
            },
        ],
    }
});

impl AgentRegistry {
    pub fn built_in() -> &'static Self {
        &BUILT_IN_REGISTRY
    }

    pub fn from_configured(configured_agents: Option<&[AgentConfig]>) -> Result<Self> {
        match configured_agents {
            Some(configured_agents) => Self::from_agent_configs(configured_agents),
            None => Ok(Self::built_in().clone()),
        }
    }

    pub fn from_agent_configs(configured_agents: &[AgentConfig]) -> Result<Self> {
        let agents = configured_agents
            .iter()
            .map(Self::definition_from_config)
            .collect::<Result<Vec<_>>>()?;
        Self::from_definitions(agents)
    }

    pub fn from_definitions(agents: Vec<AgentDefinition>) -> Result<Self> {
        let mut seen_names = HashSet::new();

        for agent in &agents {
            if !seen_names.insert(agent.name.clone()) {
                return Err(FluentCodeError::Config(format!(
                    "duplicate agent name '{}' in registry configuration",
                    agent.name
                )));
            }
        }

        Ok(Self { agents })
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    fn definition_from_config(agent: &AgentConfig) -> Result<AgentDefinition> {
        let name = agent.name.trim();
        if name.is_empty() {
            return Err(FluentCodeError::Config(
                "agent configuration requires a non-empty 'name'".to_string(),
            ));
        }

        let description = agent.description.trim();
        if description.is_empty() {
            return Err(FluentCodeError::Config(format!(
                "agent '{name}' requires a non-empty 'description'"
            )));
        }

        let system_prompt = agent.system_prompt.trim();
        if system_prompt.is_empty() {
            return Err(FluentCodeError::Config(format!(
                "agent '{name}' requires a non-empty 'system_prompt'"
            )));
        }

        Ok(AgentDefinition {
            name: name.to_string(),
            description: description.to_string(),
            system_prompt: system_prompt.to_string(),
            tier: AgentTier::default(),
            capability: AgentCapability::default(),
            cost_class: AgentCostClass::default(),
            mode: AgentMode::default(),
            temperature: None,
            tool_permissions: AgentToolPermissions {
                tools_allowed: agent.tools_allowed.clone().unwrap_or_default(),
                tools_denied: agent.tools_denied.clone().unwrap_or_default(),
            },
            delegation_targets: agent.delegation_targets.clone().unwrap_or_default(),
        })
    }
}

pub fn task_tool(agent_registry: &AgentRegistry) -> ProviderTool {
    let agent_names = agent_registry
        .agents()
        .iter()
        .map(|agent| Value::String(agent.name.clone()))
        .collect::<Vec<_>>();
    let agent_list = if agent_registry.agents().is_empty() {
        "none configured".to_string()
    } else {
        agent_registry
            .agents()
            .iter()
            .map(|agent| format!("{}: {}", agent.name, agent.description))
            .collect::<Vec<_>>()
            .join("; ")
    };

    ProviderTool {
        name: TASK_TOOL_NAME.to_string(),
        description: format!(
            "Delegate a foreground subagent task to one available agent. Available agents: {agent_list}"
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": agent_names,
                    "description": "The configured subagent to run"
                },
                "prompt": {
                    "type": "string",
                    "description": "The instruction for the delegated subagent"
                }
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false
        }),
    }
}

pub fn parse_task_request(
    agent_registry: &AgentRegistry,
    arguments: &Value,
) -> Result<TaskRequest> {
    let parsed: TaskArguments = serde_json::from_value(arguments.clone()).map_err(|error| {
        FluentCodeError::Provider(format!(
            "task arguments must be an object with string fields 'agent' and 'prompt': {error}"
        ))
    })?;

    let agent = parsed.agent.trim();
    if agent.is_empty() {
        return Err(FluentCodeError::Provider(
            "task requires a non-empty 'agent'".to_string(),
        ));
    }

    let prompt = parsed.prompt.trim();
    if prompt.is_empty() {
        return Err(FluentCodeError::Provider(
            "task requires a non-empty 'prompt'".to_string(),
        ));
    }

    if agent_registry.get(agent).is_none() {
        return Err(FluentCodeError::Provider(format!(
            "task requested unknown agent '{agent}'"
        )));
    }

    Ok(TaskRequest {
        agent: agent.to_string(),
        prompt: prompt.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use crate::config::AgentConfig;

    use super::{AgentRegistry, TASK_TOOL_NAME, parse_task_request, task_tool};

    #[test]
    fn registry_uses_built_in_defaults_when_config_absent() {
        let registry =
            AgentRegistry::from_configured(None).expect("built-in registry without config");
        let names = registry
            .agents()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["explore", "librarian"]);
    }

    #[test]
    fn configured_registry_replaces_built_in_defaults() {
        let registry = AgentRegistry::from_configured(Some(&[AgentConfig {
            name: "oracle".to_string(),
            description: "Answer architecture questions.".to_string(),
            system_prompt: "You are the oracle subagent.".to_string(),
            tools_allowed: None,
            tools_denied: None,
            delegation_targets: None,
        }]))
        .expect("custom registry");

        let names = registry
            .agents()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["oracle"]);
    }

    #[test]
    fn configured_registry_rejects_duplicate_names() {
        let error = AgentRegistry::from_configured(Some(&[
            AgentConfig {
                name: "oracle".to_string(),
                description: "Answer architecture questions.".to_string(),
                system_prompt: "You are the oracle subagent.".to_string(),
                tools_allowed: None,
                tools_denied: None,
                delegation_targets: None,
            },
            AgentConfig {
                name: "oracle".to_string(),
                description: "Review changes.".to_string(),
                system_prompt: "You are the reviewer subagent.".to_string(),
                tools_allowed: None,
                tools_denied: None,
                delegation_targets: None,
            },
        ]))
        .expect_err("duplicate names should fail");

        assert!(error.to_string().contains("duplicate agent name 'oracle'"));
    }

    #[test]
    fn task_tool_exposes_configured_agents() {
        let registry = AgentRegistry::from_configured(Some(&[test_agent_config("oracle")]))
        .expect("custom registry");
        let tool = task_tool(&registry);

        assert_eq!(tool.name, TASK_TOOL_NAME);
        assert!(tool.description.contains("oracle"));
        assert!(!tool.description.contains("explore"));
        assert_eq!(
            tool.input_schema["properties"]["agent"]["enum"],
            serde_json::json!(["oracle"])
        );
    }

    #[test]
    fn parse_task_request_accepts_known_agent_from_registry() {
        let registry = AgentRegistry::from_configured(Some(&[test_agent_config("oracle")]))
        .expect("custom registry");
        let request = parse_task_request(
            &registry,
            &serde_json::json!({
                "agent": "oracle",
                "prompt": "Inspect the provider layer"
            }),
        )
        .expect("parse valid task request");

        assert_eq!(request.agent, "oracle");
        assert_eq!(request.prompt, "Inspect the provider layer");
    }

    #[test]
    fn parse_task_request_rejects_unknown_agent_from_registry() {
        let registry = AgentRegistry::from_configured(Some(&[test_agent_config("oracle")]))
        .expect("custom registry");
        let error = parse_task_request(
            &registry,
            &serde_json::json!({
                "agent": "explore",
                "prompt": "Do work"
            }),
        )
        .expect_err("unknown agent should fail");

        assert!(error.to_string().contains("unknown agent 'explore'"));
    }

    fn test_agent_config(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            description: "Answer architecture questions.".to_string(),
            system_prompt: format!("You are the {name} subagent."),
            tools_allowed: None,
            tools_denied: None,
            delegation_targets: None,
        }
    }
}
