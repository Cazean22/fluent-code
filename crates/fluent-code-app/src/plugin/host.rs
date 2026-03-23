use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::error::{FluentCodeError, Result};

use super::manifest::PluginManifest;
use super::registry::PluginToolRegistration;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
    });
}

#[derive(Clone)]
pub struct WasmPluginExecutor {
    engine: Engine,
}

struct StoreState {
    wasi: WasiCtx,
    table: wasmtime::component::ResourceTable,
}

impl StoreState {
    fn new() -> Self {
        Self {
            wasi: WasiCtxBuilder::new().build(),
            table: wasmtime::component::ResourceTable::new(),
        }
    }
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasmPluginExecutor {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|error| {
            FluentCodeError::Plugin(format!("failed to initialize wasm engine: {error}"))
        })?;
        Ok(Self { engine })
    }

    pub fn validate_component(&self, path: &Path) -> Result<()> {
        let bytes = fs::read(path).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to read plugin component '{}': {error}",
                path.display()
            ))
        })?;
        Component::from_binary(&self.engine, &bytes).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to validate plugin component '{}': {error}",
                path.display()
            ))
        })?;
        Ok(())
    }

    pub fn validate_manifest(&self, manifest: &PluginManifest) -> Result<()> {
        manifest.supports_host_capabilities()?;

        if let Some(expected_sha256) = &manifest.component_sha256 {
            let bytes = fs::read(&manifest.component_path).map_err(|error| {
                FluentCodeError::Plugin(format!(
                    "failed to read plugin component '{}': {error}",
                    manifest.component_path.display()
                ))
            })?;
            let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
            if &actual_sha256 != expected_sha256 {
                return Err(FluentCodeError::Plugin(format!(
                    "plugin '{}' component sha256 mismatch: expected '{}', got '{}'",
                    manifest.id, expected_sha256, actual_sha256
                )));
            }
        }

        self.validate_component(&manifest.component_path)
    }

    pub fn execute(
        &self,
        registration: &PluginToolRegistration,
        input_json: &str,
    ) -> Result<String> {
        if !registration.component_path.exists() {
            return Err(FluentCodeError::Plugin(format!(
                "plugin '{}' component '{}' is not accessible",
                registration.plugin_id,
                registration.component_path.display()
            )));
        }

        let bytes = fs::read(&registration.component_path).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to read plugin component '{}': {error}",
                registration.component_path.display()
            ))
        })?;
        let component = Component::from_binary(&self.engine, &bytes).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to load plugin component '{}': {error}",
                registration.component_path.display()
            ))
        })?;

        let mut store = Store::new(&self.engine, StoreState::new());
        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|error| {
            FluentCodeError::Plugin(format!(
                "failed to add wasi imports to plugin linker: {error}"
            ))
        })?;
        let instance =
            bindings::Plugin::instantiate(&mut store, &component, &linker).map_err(|error| {
                FluentCodeError::Plugin(format!(
                    "failed to instantiate plugin component '{}': {error}",
                    registration.component_path.display()
                ))
            })?;

        let result = instance
            .call_invoke(&mut store, &registration.tool_name, input_json)
            .map_err(|error| {
                FluentCodeError::Plugin(format!(
                    "plugin '{}' trapped while executing tool '{}': {error}",
                    registration.plugin_id, registration.tool_name
                ))
            })?;

        result.map_err(|error| {
            FluentCodeError::Plugin(format!(
                "plugin '{}' failed while executing tool '{}': {error}",
                registration.plugin_id, registration.tool_name
            ))
        })
    }
}
