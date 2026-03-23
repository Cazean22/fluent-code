# Plugin Author Guide

This repository supports a v1 WASM component-based tool plugin system.

Plugins are discovered from two roots:

- Project-local plugins: `<project>/.fluent-code/plugins/<plugin-dir>/plugin.toml`
- Global plugins: `<data_dir>/plugins/<plugin-dir>/plugin.toml`

If `data_dir` is left at the default `.fluent-code`, project-local and global plugin paths collapse to the same on-disk location. You can override that with the `[plugins]` config section shown in `fluent-code.example.toml`.

## Directory layout

Each plugin lives in its own directory. The minimum layout is:

```text
your-plugin/
├── plugin.toml
└── dist/
    └── plugin.wasm
```

The plugin is discovered if the directory contains `plugin.toml`.

## Minimal manifest

```toml
[plugin]
manifest_version = 1
id = "example.echo"
name = "Echo Plugin"
version = "0.1.0"
api_version = "0.1.0"
description = "Simple text echo tool"

[component]
path = "dist/plugin.wasm"

[[tools]]
name = "plugin_echo"
description = "Echo text back to the caller"
input_schema = { type = "object", properties = { text = { type = "string" } }, required = ["text"], additionalProperties = false }
enabled = true
requires_approval = true
timeout_ms = 30000
max_output_bytes = 65536
```

## Manifest fields

### `[plugin]`

- `manifest_version`: optional, defaults to `1`
- `id`: required lowercase identifier like `example.echo`
- `name`: required display name
- `version`: required semver-like version
- `api_version`: required and must currently equal `0.1.0`
- `description`, `license`, `homepage`: optional metadata

### `[component]`

- `path`: required path to the compiled WASM component
- `sha256`: optional lowercase hex checksum for the component binary

### `[runtime]`

These values are optional because the current host defaults already match the only supported runtime shape:

```toml
[runtime]
abi = "wasm-component"
wit_world = "plugin"
wasi = "p2"
```

Any other value is rejected during plugin validation.

### `[host]`

Optional host compatibility requirements:

```toml
[host]
min_fluent_code_version = "0.1.0"
max_fluent_code_version = "0.9.0"
platforms = ["macos-aarch64"]
```

Use this if your plugin only supports certain app versions or platforms.

### `[capabilities]`

The manifest supports declared capabilities:

```toml
[capabilities]
filesystem = "none"
network = false
process = false
environment = []
```

Tool-level overrides are also supported under `[tools.capabilities]`.

## Current v1 limitation

The manifest can describe richer capabilities, but the current host only accepts the zero-capability form:

- `filesystem = "none"`
- `network = false`
- `process = false`
- `environment = []`

`workspace_read` is a recognized manifest enum value, but it is not granted by the current host yet, so declaring it causes the plugin to be disabled during validation.

In other words: today, capabilities are validated declarations, not granted sandbox permissions.

## Tool policy fields

Each `[[tools]]` entry supports:

- `enabled`: if `false`, the tool is not advertised to the model
- `requires_approval`: if `false`, the tool auto-approves and runs immediately
- `timeout_ms`: runtime execution timeout for this tool
- `max_output_bytes`: maximum output size before the host rejects the result

## Naming rules

- `plugin.id` must use lowercase segments separated by `.`, `_`, or `-`
- tool names must look like `snake_case`, for example `plugin_echo`
- plugin tools must not reuse built-in tool names
- if project and global plugins export the same tool name, the project plugin wins

## Example fixture

See `examples/plugins/echo/` for a copyable example plugin layout and manifest.
