use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn project_prompt_turn_preserves_root_grouping_with_cached_root_lookup() {
    run_exact_acp_libtest(
        "mapping::tests::project_prompt_turn_preserves_root_grouping_with_cached_root_lookup",
    );
}

#[test]
fn delegated_child_projection_ignores_broken_lineage_with_cached_lookup() {
    run_exact_acp_libtest(
        "mapping::tests::delegated_child_projection_ignores_broken_lineage_with_cached_lookup",
    );
}

fn run_exact_acp_libtest(module_qualified_test_name: &str) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let isolated_target_dir = unique_temp_dir("fluent-code-acp-cached-root-exact-wrapper");
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
