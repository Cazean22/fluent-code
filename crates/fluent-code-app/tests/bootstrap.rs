use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fluent_code_app::bootstrap::BootstrapContext;
use fluent_code_app::config::{
    AcpConfig, AcpSessionDefaultsConfig, Config, LoggingConfig, LoggingFileConfig,
    LoggingStderrConfig, ModelConfig, PluginConfig,
};
#[test]
fn bootstrap_builds_runtime_and_registries() {
    let root = unique_test_dir();
    let context = BootstrapContext::from_config(test_config(&root)).expect("bootstrap context");

    assert_eq!(context.config.data_dir, root.join(".fluent-code"));
    assert_eq!(context.plugin_load_snapshot.plugin_count(), 0);
    assert_eq!(context.plugin_load_snapshot.warning_count(), 0);
    assert!(context.store.load_or_create_latest().is_ok());

    cleanup(root);
}

#[test]
fn bootstrap_propagates_provider_configuration_errors() {
    let root = unique_test_dir();
    let mut config = test_config(&root);
    config.model.provider = "does-not-exist".to_string();

    let err = match BootstrapContext::from_config(config) {
        Ok(_) => panic!("unsupported provider should fail bootstrap"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "config error: does-not-exist");

    cleanup(root);
}

fn test_config(root: &Path) -> Config {
    let data_dir = root.join(".fluent-code");

    Config {
        config_path: None,
        data_dir: data_dir.clone(),
        logging: LoggingConfig {
            file: LoggingFileConfig {
                enabled: false,
                path: data_dir.join("logs/fluent-code.log"),
                level: "info".to_string(),
            },
            stderr: LoggingStderrConfig {
                enabled: false,
                level: "info".to_string(),
            },
        },
        model: ModelConfig {
            provider: "mock".to_string(),
            model: "gpt-4.1-mini".to_string(),
            reasoning_effort: None,
            system_prompt: "You are a helpful coding assistant.".to_string(),
        },
        agents: None,
        plugins: PluginConfig {
            enable_project_plugins: false,
            enable_global_plugins: false,
            project_dir: root.join("plugins/project"),
            global_dir: root.join("plugins/global"),
        },
        acp: AcpConfig {
            protocol_version: 1,
            auth_methods: vec![],
            session_defaults: AcpSessionDefaultsConfig {
                system_prompt: "You are a helpful coding assistant.".to_string(),
                reasoning_effort: None,
            },
        },
        model_providers: HashMap::new(),
    }
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();

    std::env::temp_dir().join(format!("fluent-code-bootstrap-test-{nanos}"))
}

fn cleanup(path: PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}
