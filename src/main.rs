// use fluent_code::{Result, run_default_entrypoint};
use client::{Result, run};

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}
