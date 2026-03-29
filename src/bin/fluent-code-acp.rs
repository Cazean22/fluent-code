use fluent_code::{Result, run_acp_entrypoint};

#[tokio::main]
async fn main() -> Result<()> {
    run_acp_entrypoint().await
}
