use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{FluentCodeError, Result};

pub const TOOL_PLUGIN_API_VERSION: &str = "0.1.0";
const PLUGIN_MANIFEST_FILE: &str = "plugin.toml";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 65_536;
const CURRENT_PLATFORM: &str = "macos-aarch64";
const CURRENT_FLUENT_CODE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub description: String,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub component_path: PathBuf,
    pub component_sha256: Option<String>,
    pub runtime: PluginRuntime,
    pub capabilities: PluginCapabilities,
    pub host: PluginHostRequirements,
    pub tools: Vec<PluginToolManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRuntime {
    pub abi: String,
    pub wit_world: String,
    pub wasi: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilesystemCapability {
    None,
    WorkspaceRead,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCapabilities {
    pub filesystem: FilesystemCapability,
    pub network: bool,
    pub process: bool,
    pub environment: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostRequirements {
    pub min_fluent_code_version: Option<String>,
    pub max_fluent_code_version: Option<String>,
    pub platforms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginToolManifest {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub enabled: bool,
    pub requires_approval: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub capabilities: PluginCapabilities,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifest {
    plugin: RawPluginMetadata,
    component: RawComponentMetadata,
    #[serde(default)]
    runtime: RawRuntimeMetadata,
    #[serde(default)]
    capabilities: RawCapabilities,
    #[serde(default)]
    host: RawHostRequirements,
    #[serde(default)]
    tools: Vec<RawPluginToolManifest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginMetadata {
    manifest_version: Option<u32>,
    id: String,
    name: String,
    version: String,
    api_version: String,
    description: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComponentMetadata {
    path: PathBuf,
    sha256: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntimeMetadata {
    abi: Option<String>,
    wit_world: Option<String>,
    wasi: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilities {
    filesystem: Option<String>,
    network: Option<bool>,
    process: Option<bool>,
    #[serde(default)]
    environment: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHostRequirements {
    min_fluent_code_version: Option<String>,
    max_fluent_code_version: Option<String>,
    #[serde(default)]
    platforms: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginToolManifest {
    name: String,
    description: Option<String>,
    input_schema: Value,
    enabled: Option<bool>,
    requires_approval: Option<bool>,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<usize>,
    #[serde(default)]
    capabilities: RawCapabilities,
}

impl PluginManifest {
    pub fn manifest_path(plugin_dir: &Path) -> PathBuf {
        plugin_dir.join(PLUGIN_MANIFEST_FILE)
    }

    pub fn load_from_dir(plugin_dir: &Path) -> Result<Self> {
        let manifest_path = Self::manifest_path(plugin_dir);
        let contents = fs::read_to_string(&manifest_path).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to read plugin manifest '{}': {error}",
                manifest_path.display()
            ))
        })?;

        let raw: RawPluginManifest = toml::from_str(&contents).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to parse plugin manifest '{}': {error}",
                manifest_path.display()
            ))
        })?;

        Self::from_raw(raw, plugin_dir)
    }

    fn from_raw(raw: RawPluginManifest, plugin_dir: &Path) -> Result<Self> {
        let manifest_version = raw.plugin.manifest_version.unwrap_or(1);
        if manifest_version != 1 {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' declares unsupported manifest_version '{}', expected '1'",
                raw.plugin.id, manifest_version
            )));
        }

        let plugin_id = validate_plugin_id(&raw.plugin.id)?;
        let plugin_name = require_non_empty("plugin.name", raw.plugin.name)?;
        let plugin_version = validate_semverish("plugin.version", raw.plugin.version)?;

        if raw.plugin.api_version != TOOL_PLUGIN_API_VERSION {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' declares unsupported api_version '{}', expected '{}'",
                plugin_id, raw.plugin.api_version, TOOL_PLUGIN_API_VERSION
            )));
        }

        let description = normalize_optional_string(raw.plugin.description);
        let license = normalize_optional_string(raw.plugin.license);
        let homepage = normalize_optional_string(raw.plugin.homepage);
        if let Some(ref homepage) = homepage
            && !(homepage.starts_with("https://") || homepage.starts_with("http://"))
        {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' declares invalid homepage '{}'",
                plugin_id, homepage
            )));
        }

        let component_path = if raw.component.path.is_absolute() {
            raw.component.path
        } else {
            plugin_dir.join(raw.component.path)
        };

        let component_sha256 = raw
            .component
            .sha256
            .map(require_lower_hex_sha256)
            .transpose()?;

        let runtime = PluginRuntime {
            abi: raw
                .runtime
                .abi
                .unwrap_or_else(|| "wasm-component".to_string()),
            wit_world: raw
                .runtime
                .wit_world
                .unwrap_or_else(|| "plugin".to_string()),
            wasi: raw.runtime.wasi.unwrap_or_else(|| "p2".to_string()),
        };

        if runtime.abi != "wasm-component" {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' requires unsupported runtime.abi '{}'",
                plugin_id, runtime.abi
            )));
        }
        if runtime.wit_world != "plugin" {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' requires unsupported runtime.wit_world '{}'",
                plugin_id, runtime.wit_world
            )));
        }
        if runtime.wasi != "p2" {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' requires unsupported runtime.wasi '{}'",
                plugin_id, runtime.wasi
            )));
        }

        let capabilities = PluginCapabilities::from_raw(raw.capabilities)?;
        let host = PluginHostRequirements::from_raw(raw.host)?;
        host.validate(&plugin_id)?;

        if raw.tools.is_empty() {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' must declare at least one tool",
                plugin_id
            )));
        }

        let mut tool_names = BTreeSet::new();
        let mut tools = Vec::with_capacity(raw.tools.len());
        for tool in raw.tools {
            let tool_name = validate_tool_name(&tool.name)?;
            if !tool_names.insert(tool_name.clone()) {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' declares duplicate tool name '{}'",
                    plugin_id, tool_name
                )));
            }

            validate_input_schema(&plugin_id, &tool_name, &tool.input_schema)?;
            let tool_capabilities =
                PluginCapabilities::from_tool_raw(&capabilities, tool.capabilities).map_err(
                    |message| {
                        FluentCodeError::Plugin(format!(
                            "plugin '{}' tool '{}' {message}",
                            plugin_id, tool_name
                        ))
                    },
                )?;

            let timeout_ms = tool.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
            if !(1..=300_000).contains(&timeout_ms) {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' tool '{}' declares invalid timeout_ms '{}'",
                    plugin_id, tool_name, timeout_ms
                )));
            }

            let max_output_bytes = tool.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
            if !(256..=1_048_576).contains(&max_output_bytes) {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' tool '{}' declares invalid max_output_bytes '{}'",
                    plugin_id, tool_name, max_output_bytes
                )));
            }

            tools.push(PluginToolManifest {
                name: tool_name,
                description: normalize_optional_string(tool.description).unwrap_or_default(),
                input_schema: tool.input_schema,
                enabled: tool.enabled.unwrap_or(true),
                requires_approval: tool.requires_approval.unwrap_or(true),
                timeout_ms,
                max_output_bytes,
                capabilities: tool_capabilities,
            });
        }

        Ok(Self {
            manifest_version,
            id: plugin_id,
            name: plugin_name,
            version: plugin_version,
            api_version: raw.plugin.api_version,
            description: description.unwrap_or_default(),
            license,
            homepage,
            component_path,
            component_sha256,
            runtime,
            capabilities,
            host,
            tools,
        })
    }

    pub fn supports_host_capabilities(&self) -> Result<()> {
        ensure_supported_capabilities(&self.id, None, &self.capabilities)?;
        for tool in &self.tools {
            ensure_supported_capabilities(&self.id, Some(&tool.name), &tool.capabilities)?;
        }
        Ok(())
    }
}

impl PluginCapabilities {
    fn from_raw(raw: RawCapabilities) -> Result<Self> {
        let environment = normalize_environment(raw.environment)?;
        Ok(Self {
            filesystem: parse_filesystem_capability(raw.filesystem.as_deref())?,
            network: raw.network.unwrap_or(false),
            process: raw.process.unwrap_or(false),
            environment,
        })
    }

    fn from_tool_raw(
        plugin_capabilities: &Self,
        raw: RawCapabilities,
    ) -> std::result::Result<Self, String> {
        let capabilities = Self {
            filesystem: raw
                .filesystem
                .as_deref()
                .map(|value| parse_filesystem_capability(Some(value)))
                .transpose()
                .map_err(|error| error.to_string())?
                .unwrap_or(plugin_capabilities.filesystem),
            network: raw.network.unwrap_or(plugin_capabilities.network),
            process: raw.process.unwrap_or(plugin_capabilities.process),
            environment: if raw.environment.is_empty() {
                plugin_capabilities.environment.clone()
            } else {
                normalize_environment(raw.environment).map_err(|error| error.to_string())?
            },
        };

        if capabilities.filesystem > plugin_capabilities.filesystem {
            return Err(
                "declares filesystem capability broader than plugin.capabilities".to_string(),
            );
        }
        if capabilities.network && !plugin_capabilities.network {
            return Err("declares network access broader than plugin.capabilities".to_string());
        }
        if capabilities.process && !plugin_capabilities.process {
            return Err("declares process access broader than plugin.capabilities".to_string());
        }
        if !capabilities
            .environment
            .iter()
            .all(|value| plugin_capabilities.environment.contains(value))
        {
            return Err("declares environment access broader than plugin.capabilities".to_string());
        }

        Ok(capabilities)
    }
}

impl PluginHostRequirements {
    fn from_raw(raw: RawHostRequirements) -> Result<Self> {
        let min_fluent_code_version = raw
            .min_fluent_code_version
            .map(|value| validate_semverish("host.min_fluent_code_version", value))
            .transpose()?;
        let max_fluent_code_version = raw
            .max_fluent_code_version
            .map(|value| validate_semverish("host.max_fluent_code_version", value))
            .transpose()?;
        let platforms = raw
            .platforms
            .into_iter()
            .map(|value| require_non_empty("host.platforms[]", value))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            min_fluent_code_version,
            max_fluent_code_version,
            platforms,
        })
    }

    fn validate(&self, plugin_id: &str) -> Result<()> {
        if let Some(ref min_version) = self.min_fluent_code_version
            && compare_semverish(CURRENT_FLUENT_CODE_VERSION, min_version) < 0
        {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' requires fluent-code >= '{}'",
                plugin_id, min_version
            )));
        }
        if let Some(ref max_version) = self.max_fluent_code_version
            && compare_semverish(CURRENT_FLUENT_CODE_VERSION, max_version) > 0
        {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' requires fluent-code <= '{}'",
                plugin_id, max_version
            )));
        }
        if !self.platforms.is_empty()
            && !self
                .platforms
                .iter()
                .any(|platform| platform == CURRENT_PLATFORM)
        {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' does not support platform '{}'",
                plugin_id, CURRENT_PLATFORM
            )));
        }
        Ok(())
    }
}

fn require_non_empty(field_name: &str, value: String) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(FluentCodeError::Plugin(format!(
            "plugin manifest requires a non-empty {field_name}"
        )));
    }
    Ok(trimmed.to_string())
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn validate_plugin_id(value: &str) -> Result<String> {
    let value = value.trim();
    let regex = Regex::new(r"^[a-z0-9]+([._-][a-z0-9]+)*$").expect("valid plugin id regex");
    if !regex.is_match(value) {
        return Err(FluentCodeError::Plugin(format!(
            "plugin manifest declares invalid plugin.id '{}'",
            value
        )));
    }
    Ok(value.to_string())
}

fn validate_tool_name(value: &str) -> Result<String> {
    let value = value.trim();
    let regex = Regex::new(r"^[a-z][a-z0-9_]*$").expect("valid tool name regex");
    if !regex.is_match(value) {
        return Err(FluentCodeError::Plugin(format!(
            "plugin manifest declares invalid tool name '{}'",
            value
        )));
    }
    Ok(value.to_string())
}

fn validate_semverish(field_name: &str, value: String) -> Result<String> {
    let value = require_non_empty(field_name, value)?;
    let regex = Regex::new(r"^\d+\.\d+\.\d+([.-][0-9A-Za-z.-]+)?$").expect("valid semver regex");
    if !regex.is_match(&value) {
        return Err(FluentCodeError::Plugin(format!(
            "plugin manifest declares invalid {field_name} '{}'",
            value
        )));
    }
    Ok(value)
}

fn require_lower_hex_sha256(value: String) -> Result<String> {
    let value = value.trim().to_string();
    let regex = Regex::new(r"^[0-9a-f]{64}$").expect("valid sha256 regex");
    if !regex.is_match(&value) {
        return Err(FluentCodeError::Plugin(format!(
            "plugin manifest declares invalid component.sha256 '{}'",
            value
        )));
    }
    Ok(value)
}

fn parse_filesystem_capability(value: Option<&str>) -> Result<FilesystemCapability> {
    match value.unwrap_or("none") {
        "none" => Ok(FilesystemCapability::None),
        "workspace_read" => Ok(FilesystemCapability::WorkspaceRead),
        other => Err(FluentCodeError::Plugin(format!(
            "plugin manifest declares unsupported filesystem capability '{}'",
            other
        ))),
    }
}

fn normalize_environment(environment: Vec<String>) -> Result<Vec<String>> {
    let regex = Regex::new(r"^[A-Z][A-Z0-9_]*$").expect("valid env regex");
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();

    for value in environment {
        let trimmed = require_non_empty("capabilities.environment[]", value)?;
        if !regex.is_match(&trimmed) {
            return Err(FluentCodeError::Plugin(format!(
                "plugin manifest declares invalid environment capability '{}'",
                trimmed
            )));
        }
        if seen.insert(trimmed.clone()) {
            normalized.push(trimmed);
        }
    }

    Ok(normalized)
}

fn ensure_supported_capabilities(
    plugin_id: &str,
    tool_name: Option<&str>,
    capabilities: &PluginCapabilities,
) -> Result<()> {
    let subject = tool_name
        .map(|tool_name| format!("plugin '{}' tool '{}'", plugin_id, tool_name))
        .unwrap_or_else(|| format!("plugin '{}'", plugin_id));

    if capabilities.filesystem != FilesystemCapability::None {
        return Err(FluentCodeError::Plugin(format!(
            "{subject} requests unsupported filesystem capability '{:?}'",
            capabilities.filesystem
        )));
    }
    if capabilities.network {
        return Err(FluentCodeError::Plugin(format!(
            "{subject} requests unsupported network capability"
        )));
    }
    if capabilities.process {
        return Err(FluentCodeError::Plugin(format!(
            "{subject} requests unsupported process capability"
        )));
    }
    if !capabilities.environment.is_empty() {
        return Err(FluentCodeError::Plugin(format!(
            "{subject} requests unsupported environment capability"
        )));
    }
    Ok(())
}

fn validate_input_schema(plugin_id: &str, tool_name: &str, input_schema: &Value) -> Result<()> {
    let Some(schema_object) = input_schema.as_object() else {
        return Err(FluentCodeError::Plugin(format!(
            "plugin '{}' tool '{}' must declare input_schema as an object",
            plugin_id, tool_name
        )));
    };

    if let Some(schema_type) = schema_object.get("type").and_then(Value::as_str)
        && schema_type != "object"
    {
        return Err(FluentCodeError::Plugin(format!(
            "plugin '{}' tool '{}' must declare input_schema.type = 'object'",
            plugin_id, tool_name
        )));
    }

    if let Some(required) = schema_object.get("required") {
        let Some(required_items) = required.as_array() else {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' tool '{}' must declare input_schema.required as an array",
                plugin_id, tool_name
            )));
        };
        let mut seen = BTreeSet::new();
        for item in required_items {
            let Some(required_name) = item.as_str() else {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' tool '{}' must declare required fields as strings",
                    plugin_id, tool_name
                )));
            };
            if !seen.insert(required_name.to_string()) {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' tool '{}' declares duplicate required field '{}'",
                    plugin_id, tool_name, required_name
                )));
            }
        }
    }

    Ok(())
}

fn compare_semverish(left: &str, right: &str) -> i8 {
    let left_parts = parse_semverish(left);
    let right_parts = parse_semverish(right);
    for (left, right) in left_parts.into_iter().zip(right_parts) {
        match left.cmp(&right) {
            std::cmp::Ordering::Less => return -1,
            std::cmp::Ordering::Greater => return 1,
            std::cmp::Ordering::Equal => {}
        }
    }
    0
}

fn parse_semverish(value: &str) -> [u64; 3] {
    let core = value.split(['-', '+']).next().unwrap_or(value);
    let mut parts = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    [
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use uuid::Uuid;

    use super::{FilesystemCapability, PluginManifest, TOOL_PLUGIN_API_VERSION};

    #[test]
    fn parses_plugin_manifest_with_richer_defaults() {
        let plugin_dir = unique_test_dir();
        fs::create_dir_all(plugin_dir.join("dist")).expect("create plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            format!(
                r#"
[plugin]
id = "example.echo"
name = "Echo Plugin"
version = "0.1.0"
api_version = "{TOOL_PLUGIN_API_VERSION}"
description = "Echoes input"

[component]
path = "dist/plugin.wasm"

[[tools]]
name = "plugin_echo"
description = "Echo text"
input_schema = {{ type = "object", required = ["text"] }}
"#
            ),
        )
        .expect("write manifest");

        let manifest = PluginManifest::load_from_dir(&plugin_dir).expect("load manifest");

        assert_eq!(manifest.manifest_version, 1);
        assert_eq!(manifest.id, "example.echo");
        assert_eq!(manifest.runtime.abi, "wasm-component");
        assert_eq!(manifest.capabilities.filesystem, FilesystemCapability::None);
        assert_eq!(manifest.tools.len(), 1);
        assert!(manifest.tools[0].enabled);
        assert!(manifest.tools[0].requires_approval);
        assert_eq!(manifest.tools[0].timeout_ms, 30_000);
        assert_eq!(manifest.component_path, plugin_dir.join("dist/plugin.wasm"));

        cleanup(&plugin_dir);
    }

    #[test]
    fn rejects_incompatible_api_version() {
        let plugin_dir = write_manifest(
            r#"
[plugin]
id = "example.echo"
name = "Echo Plugin"
version = "0.1.0"
api_version = "9.9.9"

[component]
path = "plugin.wasm"

[[tools]]
name = "plugin_echo"
input_schema = { type = "object" }
"#,
        );

        let error = PluginManifest::load_from_dir(&plugin_dir).expect_err("manifest should fail");

        assert!(error.to_string().contains("unsupported api_version"));
        cleanup(&plugin_dir);
    }

    #[test]
    fn rejects_duplicate_tool_names() {
        let plugin_dir = write_manifest(
            format!(
                r#"
[plugin]
id = "example.echo"
name = "Echo Plugin"
version = "0.1.0"
api_version = "{TOOL_PLUGIN_API_VERSION}"

[component]
path = "plugin.wasm"

[[tools]]
name = "plugin_echo"
input_schema = {{ type = "object" }}

[[tools]]
name = "plugin_echo"
input_schema = {{ type = "object" }}
"#
            )
            .as_str(),
        );

        let error = PluginManifest::load_from_dir(&plugin_dir)
            .expect_err("duplicate tool names should fail");

        assert!(error.to_string().contains("duplicate tool name"));
        cleanup(&plugin_dir);
    }

    #[test]
    fn rejects_broader_tool_capabilities_than_plugin() {
        let plugin_dir = write_manifest(
            format!(
                r#"
[plugin]
id = "example.echo"
name = "Echo Plugin"
version = "0.1.0"
api_version = "{TOOL_PLUGIN_API_VERSION}"

[component]
path = "plugin.wasm"

[capabilities]
network = false

[[tools]]
name = "plugin_echo"
input_schema = {{ type = "object" }}

[tools.capabilities]
network = true
"#
            )
            .as_str(),
        );

        let error = PluginManifest::load_from_dir(&plugin_dir)
            .expect_err("broader tool capabilities should fail");
        assert!(
            error
                .to_string()
                .contains("broader than plugin.capabilities")
        );
        cleanup(&plugin_dir);
    }

    #[test]
    fn rejects_unsupported_host_platform() {
        let plugin_dir = write_manifest(
            format!(
                r#"
[plugin]
id = "example.echo"
name = "Echo Plugin"
version = "0.1.0"
api_version = "{TOOL_PLUGIN_API_VERSION}"

[component]
path = "plugin.wasm"

[host]
platforms = ["linux-x86_64"]

[[tools]]
name = "plugin_echo"
input_schema = {{ type = "object" }}
"#
            )
            .as_str(),
        );

        let error = PluginManifest::load_from_dir(&plugin_dir)
            .expect_err("unsupported platform should fail");
        assert!(error.to_string().contains("does not support platform"));
        cleanup(&plugin_dir);
    }

    fn write_manifest(contents: &str) -> std::path::PathBuf {
        let plugin_dir = unique_test_dir();
        fs::create_dir_all(&plugin_dir).expect("create plugin dir");
        fs::write(plugin_dir.join("plugin.toml"), contents).expect("write manifest");
        plugin_dir
    }

    fn unique_test_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "fluent-code-plugin-manifest-test-{}",
            Uuid::new_v4()
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
