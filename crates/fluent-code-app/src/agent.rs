use std::collections::HashSet;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use fluent_code_provider::ProviderTool;

use crate::config::AgentConfig;
use crate::error::{FluentCodeError, Result};

pub const TASK_TOOL_NAME: &str = "task";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuiltInAgentDefinition {
    name: &'static str,
    description: &'static str,
    system_prompt: &'static str,
}

const BUILT_IN_AGENTS: &[BuiltInAgentDefinition] = &[
    BuiltInAgentDefinition {
        name: "explore",
        description: "Investigate the repository, trace implementations, and report precise findings.",
        system_prompt: "You are the explore subagent. Investigate the repository carefully, follow existing code patterns, and answer with concrete findings grounded in the code you read. Focus on discovery, not implementation.",
    },
    BuiltInAgentDefinition {
        name: "librarian",
        description: "Gather code-oriented reference material and summarize the most relevant details.",
        system_prompt: "You are the librarian subagent. Read the available project context carefully and return a concise, well-organized reference summary that helps the parent agent continue implementation accurately.",
    },
];

static BUILT_IN_REGISTRY: LazyLock<AgentRegistry> = LazyLock::new(|| AgentRegistry {
    agents: BUILT_IN_AGENTS
        .iter()
        .map(|agent| AgentDefinition {
            name: agent.name.to_string(),
            description: agent.description.to_string(),
            system_prompt: agent.system_prompt.to_string(),
        })
        .collect(),
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
            },
            AgentConfig {
                name: "oracle".to_string(),
                description: "Review changes.".to_string(),
                system_prompt: "You are the reviewer subagent.".to_string(),
            },
        ]))
        .expect_err("duplicate names should fail");

        assert!(error.to_string().contains("duplicate agent name 'oracle'"));
    }

    #[test]
    fn task_tool_exposes_configured_agents() {
        let registry = AgentRegistry::from_configured(Some(&[AgentConfig {
            name: "oracle".to_string(),
            description: "Answer architecture questions.".to_string(),
            system_prompt: "You are the oracle subagent.".to_string(),
        }]))
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
        let registry = AgentRegistry::from_configured(Some(&[AgentConfig {
            name: "oracle".to_string(),
            description: "Answer architecture questions.".to_string(),
            system_prompt: "You are the oracle subagent.".to_string(),
        }]))
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
        let registry = AgentRegistry::from_configured(Some(&[AgentConfig {
            name: "oracle".to_string(),
            description: "Answer architecture questions.".to_string(),
            system_prompt: "You are the oracle subagent.".to_string(),
        }]))
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
}
