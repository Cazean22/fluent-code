use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::PluginConfig;
use crate::error::{FluentCodeError, Result};

use super::manifest::PluginManifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryScope {
    Global,
    Project,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredPlugin {
    pub scope: DiscoveryScope,
    pub plugin_dir: PathBuf,
    pub manifest: PluginManifest,
}

#[derive(Debug, Default)]
pub struct PluginDiscovery {
    pub plugins: Vec<DiscoveredPlugin>,
    pub warnings: Vec<String>,
}

pub fn discover_plugins(config: &PluginConfig) -> PluginDiscovery {
    let mut discovery = PluginDiscovery::default();

    if config.enable_global_plugins {
        discover_in_root(
            &config.global_dir,
            DiscoveryScope::Global,
            &mut discovery.plugins,
            &mut discovery.warnings,
        );
    }

    if config.enable_project_plugins {
        discover_in_root(
            &config.project_dir,
            DiscoveryScope::Project,
            &mut discovery.plugins,
            &mut discovery.warnings,
        );
    }

    discovery.plugins.sort_by(|left, right| {
        left.scope
            .cmp(&right.scope)
            .then_with(|| left.plugin_dir.cmp(&right.plugin_dir))
    });

    discovery
}

fn discover_in_root(
    root: &Path,
    scope: DiscoveryScope,
    plugins: &mut Vec<DiscoveredPlugin>,
    warnings: &mut Vec<String>,
) {
    let entries = match read_sorted_entries(root) {
        Ok(entries) => entries,
        Err(FluentCodeError::Io(ref io)) if io.kind() == std::io::ErrorKind::NotFound => {
            return;
        }
        Err(error) => {
            warnings.push(format!(
                "failed to read plugin directory '{}': {error}",
                root.display()
            ));
            return;
        }
    };

    for plugin_dir in entries {
        if !plugin_dir.is_dir() {
            continue;
        }

        let manifest_path = PluginManifest::manifest_path(&plugin_dir);
        if !manifest_path.exists() {
            continue;
        }

        match PluginManifest::load_from_dir(&plugin_dir) {
            Ok(manifest) => plugins.push(DiscoveredPlugin {
                scope,
                plugin_dir,
                manifest,
            }),
            Err(error) => warnings.push(error.to_string()),
        }
    }
}

fn read_sorted_entries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(root)
        .map_err(FluentCodeError::Io)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{DiscoveryScope, discover_plugins};
    use crate::config::PluginConfig;
    use crate::plugin::manifest::TOOL_PLUGIN_API_VERSION;

    #[test]
    fn discovers_project_plugins_after_global_plugins() {
        let base = unique_test_dir();
        let global_dir = base.join("global");
        let project_dir = base.join("project");
        write_plugin(
            &global_dir.join("echo-global"),
            "global.echo",
            "plugin_echo",
        );
        write_plugin(
            &project_dir.join("echo-project"),
            "project.echo",
            "plugin_echo",
        );

        let config = PluginConfig {
            enable_project_plugins: true,
            enable_global_plugins: true,
            project_dir,
            global_dir,
        };

        let discovery = discover_plugins(&config);

        assert!(discovery.warnings.is_empty());
        assert_eq!(discovery.plugins.len(), 2);
        assert_eq!(discovery.plugins[0].scope, DiscoveryScope::Global);
        assert_eq!(discovery.plugins[1].scope, DiscoveryScope::Project);

        cleanup(&base);
    }

    #[test]
    fn continues_when_one_plugin_manifest_is_invalid() {
        let base = unique_test_dir();
        let global_dir = base.join("global");
        let project_dir = base.join("project");
        write_plugin(&global_dir.join("valid"), "global.echo", "plugin_echo");
        fs::create_dir_all(project_dir.join("broken")).expect("create broken plugin dir");
        fs::write(
            project_dir.join("broken/plugin.toml"),
            "[plugin]\nid='broken'\n",
        )
        .expect("write invalid plugin manifest");

        let config = PluginConfig {
            enable_project_plugins: true,
            enable_global_plugins: true,
            project_dir,
            global_dir,
        };

        let discovery = discover_plugins(&config);

        assert_eq!(discovery.plugins.len(), 1);
        assert_eq!(discovery.warnings.len(), 1);

        cleanup(&base);
    }

    fn write_plugin(plugin_dir: &std::path::Path, plugin_id: &str, tool_name: &str) {
        fs::create_dir_all(plugin_dir).expect("create plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            format!(
                r#"
[plugin]
id = "{plugin_id}"
name = "{plugin_id}"
version = "0.1.0"
api_version = "{TOOL_PLUGIN_API_VERSION}"

[component]
path = "plugin.wasm"

[[tools]]
name = "{tool_name}"
input_schema = {{ type = "object" }}
"#
            ),
        )
        .expect("write plugin manifest");
    }

    fn unique_test_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();

        std::env::temp_dir().join(format!("fluent-code-plugin-discovery-test-{nanos}"))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = fs::remove_dir_all(path);
    }
}
