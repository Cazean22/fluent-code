use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn live_projection_watermark_matches_full_projection_for_monotonic_updates() {
    run_exact_acp_libtest(
        "server::tests::live_projection_watermark_matches_full_projection_for_monotonic_updates",
    );
}

#[test]
fn live_projection_falls_back_to_full_projection_on_stream_reopen() {
    run_exact_acp_libtest(
        "server::tests::live_projection_falls_back_to_full_projection_on_stream_reopen",
    );
}

#[test]
fn live_projection_empty_poll_does_not_emit_duplicate_text() {
    run_exact_acp_libtest("server::tests::live_projection_empty_poll_does_not_emit_duplicate_text");
}

#[test]
fn replay_projection_ignores_live_watermark_state() {
    run_exact_acp_libtest("server::tests::replay_projection_ignores_live_watermark_state");
}

fn run_exact_acp_libtest(module_qualified_test_name: &str) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let isolated_target_dir = unique_temp_dir("fluent-code-acp-task4-exact-wrapper");
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
