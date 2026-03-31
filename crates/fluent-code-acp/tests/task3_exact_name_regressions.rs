use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn session_prompt_flushes_pending_agent_delta_before_terminal_stop_reason() {
    run_exact_acp_libtest(
        "server::tests::session_prompt_flushes_pending_agent_delta_before_terminal_stop_reason",
    );
}

#[test]
fn session_prompt_preserves_many_chunk_continuity_without_duplicate_text() {
    run_exact_acp_libtest(
        "server::tests::session_prompt_preserves_many_chunk_continuity_without_duplicate_text",
    );
}

#[test]
fn contract_live_same_connection_cancel_resolves_prompt_over_stdio_loop() {
    run_exact_acp_libtest(
        "server::contract_tests::contract_live_same_connection_cancel_resolves_prompt_over_stdio_loop",
    );
}

fn run_exact_acp_libtest(module_qualified_test_name: &str) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let isolated_target_dir = unique_temp_dir("fluent-code-acp-task3-exact-wrapper");
    let output = Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CARGO_TARGET_DIR", &isolated_target_dir)
        .args([
            "test",
            "-p",
            "fluent-code-acp",
            "--lib",
            module_qualified_test_name,
            "--",
            "--exact",
        ])
        .output()
        .expect("spawn nested cargo test for module-qualified ACP libtest");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "nested ACP libtest `{module_qualified_test_name}` failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("running 1 test") || stderr.contains("running 1 test"),
        "expected nested ACP libtest `{module_qualified_test_name}` to run exactly one test\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let _ = fs::remove_dir_all(isolated_target_dir);
}

fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    let unique_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique_suffix}"))
}
