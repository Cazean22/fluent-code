use fc_tui::run;


#[tokio::main]
async fn main() -> std::result::Result<(), std::io::Error> {
    run().await.unwrap();
    Ok(())
}
