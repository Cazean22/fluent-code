#[test]
fn default_binary_uses_tui_entrypoint() {
    let main_rs = include_str!("../src/main.rs");
    let cargo_toml = include_str!("../Cargo.toml");

    assert!(
        main_rs.contains("run_default_entrypoint().await"),
        "expected src/main.rs to delegate to the TUI default entrypoint"
    );
    assert!(
        cargo_toml.contains("default-run = \"fluent-code\""),
        "expected Cargo.toml to default `cargo run -p fluent-code` to the TUI binary"
    );
    assert!(
        !main_rs.contains("fluent_code_acp::{Result, run}"),
        "expected src/main.rs to stop delegating directly to ACP startup"
    );
}

#[test]
fn acp_secondary_binary_is_wired() {
    let acp_bin = include_str!("../src/bin/fluent-code-acp.rs");

    assert!(
        acp_bin.contains("run_acp_entrypoint().await"),
        "expected the secondary ACP binary to delegate to the ACP entrypoint wrapper"
    );
}
