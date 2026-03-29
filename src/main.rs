use fluent_code::{Result, run_default_entrypoint};

#[tokio::main]
async fn main() -> Result<()> {
    run_default_entrypoint().await
}
