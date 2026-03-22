use fluent_code_app::{
    config::Config,
    error::Result,
    logging::{config_source_for_log, init_logging, path_for_log},
    runtime::Runtime,
    session::store::FsSessionStore,
};
use fluent_code_provider::ProviderClient;
use fluent_code_tui as tui;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let _logging = init_logging(&config)?;

    info!(
        config_source = %config_source_for_log(config.config_path.as_deref()),
        data_dir = %path_for_log(&config.data_dir),
        provider = %config.model.provider,
        model = %config.model.model,
        file_logging = config.logging.file.enabled,
        file_log_path = %path_for_log(&config.logging.file.path),
        file_log_level = %config.logging.file.level,
        stderr_logging = config.logging.stderr.enabled,
        stderr_log_level = %config.logging.stderr.level,
        "application startup configuration loaded"
    );

    let store = FsSessionStore::new(config.data_dir.clone());
    let session = store.load_or_create_latest()?;
    let provider = ProviderClient::new(
        &config.model.provider,
        config.model.model.clone(),
        config.model.system_prompt.clone(),
        config.model.reasoning_effort.clone(),
        config.selected_provider_config().cloned(),
    )?;
    let runtime = Runtime::new(provider);

    tui::run_app(session, store, runtime).await
}
