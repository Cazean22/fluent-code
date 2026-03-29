pub use fluent_code_acp::{FluentCodeError, Result};

pub async fn run_default_entrypoint() -> Result<()> {
    fluent_code_tui::run().await
}

pub async fn run_acp_entrypoint() -> Result<()> {
    fluent_code_acp::run().await
}
