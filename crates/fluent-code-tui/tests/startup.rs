use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_client_protocol as acp;
use chrono::Utc;
use fluent_code_app::app::RESTART_INTERRUPTED_TASK_RESULT;
use fluent_code_app::error::FluentCodeError;
use fluent_code_app::session::model::{
    Role, RunRecord, RunStatus, Session, TaskDelegationRecord, TaskDelegationStatus,
    ToolApprovalState, ToolExecutionState, ToolInvocationRecord, ToolSource, Turn,
};
use fluent_code_app::session::store::{FsSessionStore, SessionStore};
use fluent_code_tui::{
    AcpFilesystemService, AcpLaunchOptions, AcpTerminalService, SubprocessStatus, TranscriptSource,
    bootstrap_client_for_tests, initialize_default_session_for_tests,
    run_with_terminal_hooks_for_tests,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::Mutex;

struct StartupRecoveryFixture {
    session: Session,
    child_run_id: uuid::Uuid,
}

#[test]
fn startup_uses_latest_session_by_default() {
    let runtime = tokio::runtime::Runtime::new().expect("test tokio runtime");
    runtime.block_on(async {
        let _guard = startup_subprocess_test_lock().lock().await;
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create startup root");
        write_acp_subprocess_test_config(&root);

        let store = FsSessionStore::new(root.join(".fluent-code"));
        let existing = store.create_new_session().expect("create latest session");
        let acp_binary = build_acp_binary();

        {
            let _cwd_guard = CurrentDirGuard::set(&root);
            tokio::task::LocalSet::new()
                .run_until(async {
                    let runtime =
                        bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                            .await
                            .expect("bootstrap ACP client subprocess for startup load");
                    initialize_default_session_for_tests(&runtime, &root)
                        .await
                        .expect("initialize default ACP startup session");

                    let snapshot =
                        wait_for_projected_session(&runtime, &existing.id.to_string()).await;
                    assert_eq!(
                        snapshot.session.session_id.as_deref(),
                        Some(existing.id.to_string().as_str())
                    );

                    runtime
                        .shutdown()
                        .await
                        .expect("shutdown ACP subprocess after loading latest session");
                })
                .await;
        }

        cleanup(root);
    });
}

#[tokio::test]
async fn restores_terminal_when_startup_recovery_fails() {
    let root = unique_test_dir();
    let restored = AtomicBool::new(false);

    let err = run_with_terminal_hooks_for_tests(
        || {
            let backend = CrosstermBackend::new(std::io::stdout());
            Ok(Terminal::new(backend).expect("test terminal"))
        },
        |terminal| {
            restored.store(true, Ordering::SeqCst);
            drop(terminal);
            Ok(())
        },
        |_terminal| async {
            Err(FluentCodeError::Config(
                "forced startup recovery failure".to_string(),
            ))
        },
    )
    .await
    .expect_err("startup recovery should fail");

    assert_eq!(
        err.to_string(),
        "config error: forced startup recovery failure"
    );
    assert!(restored.load(Ordering::SeqCst));

    cleanup(root);
}

#[tokio::test]
async fn startup_recovery_resumes_parent_and_persists_terminalized_child() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create startup recovery root");
    write_acp_subprocess_test_config(&root);

    let store = FsSessionStore::new(root.join(".fluent-code"));
    let fixture = interrupted_delegation_fixture();
    store
        .create(&fixture.session)
        .expect("persist startup fixture");

    let acp_binary = build_acp_binary();
    {
        let _cwd_guard = CurrentDirGuard::set(&root);
        tokio::task::LocalSet::new()
            .run_until(async {
                let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                    .await
                    .expect("bootstrap ACP client subprocess for delegated startup recovery");
                initialize_default_session_for_tests(&runtime, &root)
                    .await
                    .expect("initialize default ACP startup session from latest pointer");

                let snapshot = wait_for_projected_session(&runtime, &fixture.session.id.to_string()).await;
                assert_eq!(snapshot.session.session_id.as_deref(), Some(fixture.session.id.to_string().as_str()));
                assert!(
                    snapshot
                        .tool_statuses
                        .iter()
                        .any(|status| status.tool_call_id == "task-call-1"),
                    "expected ACP replay to project the terminalized delegated tool, got: {snapshot:?}"
                );
                assert!(
                    !snapshot.transcript_rows.is_empty(),
                    "expected ACP replay to restore transcript rows for the recovered session, got: {snapshot:?}"
                );

                runtime
                    .shutdown()
                    .await
                    .expect("shutdown ACP subprocess after delegated startup recovery");
            })
            .await;
    }

    let persisted = store
        .load(&fixture.session.id)
        .expect("load recovered session after ACP startup replay");
    assert_eq!(
        persisted.tool_invocations[0].result.as_deref(),
        Some(RESTART_INTERRUPTED_TASK_RESULT)
    );
    assert_eq!(
        persisted.tool_invocations[0].delegation_status(),
        Some(TaskDelegationStatus::Failed)
    );
    assert!(matches!(
        persisted.runs.iter().find(|run| run.id == fixture.child_run_id),
        Some(run) if run.status == RunStatus::Failed
    ));

    cleanup(root);
}

#[tokio::test]
async fn startup_recovery_fails_closed_for_malformed_lineage() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create malformed-lineage startup root");
    write_acp_subprocess_test_config(&root);

    let store = FsSessionStore::new(root.join(".fluent-code"));
    let mut fixture = interrupted_delegation_fixture();
    fixture
        .session
        .runs
        .retain(|run| run.id != fixture.child_run_id);
    store
        .create(&fixture.session)
        .expect("persist malformed startup fixture");

    let acp_binary = build_acp_binary();
    {
        let _cwd_guard = CurrentDirGuard::set(&root);
        tokio::task::LocalSet::new()
            .run_until(async {
                let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                    .await
                    .expect("bootstrap ACP client subprocess for malformed-lineage replay");
                initialize_default_session_for_tests(&runtime, &root)
                    .await
                    .expect("initialize default ACP startup session from malformed latest pointer");

                let snapshot = wait_for_projected_session(&runtime, &fixture.session.id.to_string()).await;
                assert_eq!(snapshot.session.session_id.as_deref(), Some(fixture.session.id.to_string().as_str()));
                assert!(snapshot.pending_permission.is_none());
                assert!(
                    snapshot
                        .tool_statuses
                        .iter()
                        .any(|status| status.tool_call_id == "task-call-1"),
                    "expected malformed lineage replay to surface the delegated task tool, got: {snapshot:?}"
                );

                runtime
                    .shutdown()
                    .await
                    .expect("shutdown ACP subprocess after malformed-lineage replay");
            })
            .await;
    }

    let persisted = store
        .load(&fixture.session.id)
        .expect("reload malformed session");
    assert_eq!(
        persisted.tool_invocations[0].delegation_status(),
        Some(TaskDelegationStatus::Running)
    );

    cleanup(root);
}

#[tokio::test]
async fn startup_spawns_acp_subprocess_and_initializes_client() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create startup subprocess root");
    write_acp_subprocess_test_config(&root);

    let acp_binary = build_acp_binary();
    tokio::task::LocalSet::new()
        .run_until(async {
            let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                .await
                .expect("bootstrap ACP client subprocess through TUI startup path");
            let snapshot = runtime.projection_snapshot().await;
            let initialize_response = runtime.initialize_response();

            assert!(snapshot.startup_error.is_none());
            assert_eq!(
                initialize_response.protocol_version,
                acp::ProtocolVersion::V1
            );
            assert!(initialize_response.agent_capabilities.load_session);
            assert_eq!(
                initialize_response
                    .agent_info
                    .as_ref()
                    .map(|info| info.name.as_str()),
                Some("fluent-code")
            );
            assert_eq!(
                initialize_response
                    .agent_info
                    .as_ref()
                    .and_then(|info| info.title.as_deref()),
                Some("Fluent Code")
            );
            assert!(matches!(
                snapshot.subprocess.status,
                SubprocessStatus::Initialized {
                    ref binary_path,
                    ref protocol_version,
                    ..
                } if *binary_path == acp_binary && protocol_version == "1"
            ));

            runtime
                .shutdown()
                .await
                .expect("shutdown ACP subprocess after startup bootstrap");
        })
        .await;

    cleanup(root);
}

#[tokio::test]
async fn startup_reports_missing_acp_binary_cleanly() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create missing binary root");
    let missing_binary = root.join(format!(
        "missing-fluent-code-acp{}",
        std::env::consts::EXE_SUFFIX
    ));

    let error = tokio::task::LocalSet::new()
        .run_until(async {
            match bootstrap_client_for_tests(AcpLaunchOptions::new(&missing_binary, &root)).await {
                Ok(_) => {
                    panic!("missing ACP binary should fail cleanly through TUI startup path")
                }
                Err(error) => error,
            }
        })
        .await;

    assert!(matches!(error, FluentCodeError::Config(_)));
    assert!(
        error
            .to_string()
            .contains("failed to launch ACP subprocess"),
        "expected a clean missing-binary launch error, got: {error}"
    );
    assert!(
        error.to_string().contains("No such file") || error.to_string().contains("os error 2"),
        "expected missing-binary OS detail, got: {error}"
    );

    cleanup(root);
}

#[tokio::test]
async fn permission_allow_once_resumes_prompt_via_acp() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create ACP permission test root");
    write_acp_subprocess_test_config(&root);

    let acp_binary = build_acp_binary();
    tokio::task::LocalSet::new()
        .run_until(async {
            let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                .await
                .expect("bootstrap ACP client subprocess through TUI startup path");
            let new_session = runtime
                .new_session(root.display().to_string())
                .await
                .expect("create ACP session through official client");

            let (prompt_result, ()) = tokio::join!(
                runtime.prompt(
                    new_session.session_id.clone(),
                    "please use uppercase_text: hello tool"
                ),
                async {
                    let permission = wait_for_pending_permission(&runtime).await;
                    assert!(
                        permission
                            .options
                            .iter()
                            .any(|option| option.option_id == "allow_once"),
                        "expected an allow_once ACP permission option"
                    );
                    runtime
                        .select_permission_option_for_tests("allow_once")
                        .await
                        .expect("reply to ACP permission request");
                }
            );

            let prompt_result = prompt_result.expect("prompt to resume after ACP permission reply");
            assert_eq!(
                prompt_result.stop_reason,
                agent_client_protocol::StopReason::EndTurn
            );

            let snapshot = runtime.projection_snapshot().await;
            assert!(snapshot.pending_permission.is_none());
            assert!(
                snapshot
                    .tool_statuses
                    .iter()
                    .any(|status| status.status == "completed"),
                "expected ACP tool status updates to include a completed tool"
            );
            assert!(
                !snapshot.transcript_rows.is_empty(),
                "expected ACP transcript projection to receive prompt-turn updates"
            );

            runtime
                .shutdown()
                .await
                .expect("shutdown ACP subprocess after permission round trip");
        })
        .await;

    cleanup(root);
}

#[tokio::test]
async fn permission_cancel_during_request_cancels_prompt_via_acp() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create ACP cancel test root");
    write_acp_subprocess_test_config(&root);

    let acp_binary = build_acp_binary();
    tokio::task::LocalSet::new()
        .run_until(async {
            let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                .await
                .expect("bootstrap ACP client subprocess through TUI startup path");
            let new_session = runtime
                .new_session(root.display().to_string())
                .await
                .expect("create ACP session through official client");

            let (prompt_result, ()) = tokio::join!(
                runtime.prompt(
                    new_session.session_id.clone(),
                    "please use uppercase_text: cancel tool"
                ),
                async {
                    let _permission = wait_for_pending_permission(&runtime).await;
                    runtime
                        .cancel_pending_permission_for_tests()
                        .await
                        .expect("cancel ACP permission request");
                }
            );

            let prompt_result =
                prompt_result.expect("prompt to resolve after ACP permission cancel");
            assert_eq!(
                prompt_result.stop_reason,
                agent_client_protocol::StopReason::Cancelled
            );

            runtime
                .load_session(new_session.session_id.clone(), root.display().to_string())
                .await
                .expect("reload cancelled ACP session");

            let snapshot = runtime.projection_snapshot().await;
            assert!(snapshot.pending_permission.is_none());

            runtime
                .shutdown()
                .await
                .expect("shutdown ACP subprocess after permission cancel round trip");
        })
        .await;

    cleanup(root);
}

#[tokio::test]
async fn tui_prompt_flow_uses_acp_subprocess_end_to_end() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create ACP prompt-flow test root");
    write_acp_subprocess_test_config(&root);

    let acp_binary = build_acp_binary();
    tokio::task::LocalSet::new()
        .run_until(async {
            let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                .await
                .expect("bootstrap ACP client subprocess for prompt flow");
            let new_session = runtime
                .new_session(root.display().to_string())
                .await
                .expect("create ACP session for prompt flow");
            let full_response = "Mock assistant response: please stream a response over ACP";

            {
                let (prompt_result, streaming_snapshot) = tokio::join!(
                    runtime.prompt(
                        new_session.session_id.clone(),
                        "please stream a response over ACP",
                    ),
                    wait_for_in_flight_agent_transcript_content(&runtime)
                );
                let streaming_agent_rows = streaming_snapshot
                    .transcript_rows
                    .iter()
                    .filter(|row| row.source == TranscriptSource::Agent)
                    .collect::<Vec<_>>();
                assert_eq!(
                    streaming_agent_rows.len(),
                    1,
                    "expected in-flight ACP streaming to grow one agent row in place, got: {streaming_snapshot:?}"
                );
                assert!(
                    !streaming_agent_rows[0].content.is_empty(),
                    "expected in-flight ACP transcript to contain partial streamed content, got: {streaming_snapshot:?}"
                );
                assert!(
                    !streaming_agent_rows[0].content.contains(full_response),
                    "expected in-flight ACP transcript snapshot to remain partial rather than final, got: {streaming_snapshot:?}"
                );

                let prompt_result = prompt_result
                    .expect("prompt flow to complete through the ACP subprocess");
                assert_eq!(prompt_result.stop_reason, agent_client_protocol::StopReason::EndTurn);
            }

            let snapshot = wait_for_agent_transcript_content(&runtime, "Mock assistant").await;
            assert!(snapshot.pending_permission.is_none());
            let agent_rows = snapshot
                .transcript_rows
                .iter()
                .filter(|row| row.source == TranscriptSource::Agent)
                .collect::<Vec<_>>();
            assert_eq!(
                agent_rows.len(),
                1,
                "expected the ACP prompt flow to coalesce streamed agent chunks into one transcript row, got: {snapshot:?}"
            );
            let combined_agent_text = agent_rows
                .iter()
                .map(|row| row.content.as_str())
                .collect::<String>();
            assert!(
                combined_agent_text.contains(full_response),
                "expected the streamed ACP transcript to contain the final response, got: {combined_agent_text:?}"
            );
            assert!(
                agent_rows[0].content.contains(full_response),
                "expected the single coalesced ACP transcript row to contain the full response, got: {:?}",
                agent_rows[0].content
            );

            runtime
                .shutdown()
                .await
                .expect("shutdown ACP subprocess after prompt flow test");
        })
        .await;

    cleanup(root);
}

#[tokio::test]
async fn tui_permission_and_tool_lifecycle_use_acp_end_to_end() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create ACP permission lifecycle root");
    write_acp_subprocess_test_config(&root);

    let acp_binary = build_acp_binary();
    tokio::task::LocalSet::new()
        .run_until(async {
            let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                .await
                .expect("bootstrap ACP client subprocess for permission lifecycle");
            let new_session = runtime
                .new_session(root.display().to_string())
                .await
                .expect("create ACP session for permission lifecycle");

            let (prompt_result, ()) = tokio::join!(
                runtime.prompt(
                    new_session.session_id.clone(),
                    "please use uppercase_text: hello tool"
                ),
                async {
                    let permission = wait_for_pending_permission(&runtime).await;
                    assert!(
                        permission.options.iter().any(|option| option.option_id == "allow_once"),
                        "expected the ACP tool flow to surface an allow_once permission option"
                    );
                    wait_for_tool_status(&runtime, "pending").await;
                    runtime
                        .select_permission_option_for_tests("allow_once")
                        .await
                        .expect("reply to ACP permission request");
                }
            );

            let prompt_result = prompt_result
                .expect("permission/tool lifecycle prompt to complete through the ACP subprocess");
            assert_eq!(prompt_result.stop_reason, agent_client_protocol::StopReason::EndTurn);

            let snapshot = runtime.projection_snapshot().await;
            assert!(snapshot.pending_permission.is_none());
            assert!(
                snapshot.tool_statuses.iter().any(|status| status.status == "completed"),
                "expected ACP tool lifecycle projection to include a completed tool, got: {snapshot:?}"
            );
            let combined_agent_text = snapshot
                .transcript_rows
                .iter()
                .filter(|row| row.source == TranscriptSource::Agent)
                .map(|row| row.content.as_str())
                .collect::<String>();
            assert!(
                combined_agent_text.contains("HELLO TOOL"),
                "expected ACP transcript to include the resumed tool result, got: {combined_agent_text:?}"
            );

            runtime
                .shutdown()
                .await
                .expect("shutdown ACP subprocess after permission lifecycle test");
        })
        .await;

    cleanup(root);
}

#[tokio::test]
async fn tui_filesystem_and_terminal_capabilities_use_acp_end_to_end() {
    let _guard = startup_subprocess_test_lock().lock().await;
    let root = unique_test_dir();
    fs::create_dir_all(root.join("notes")).expect("create ACP capability root");
    fs::write(root.join("marker.txt"), "marker").expect("write terminal capability marker file");
    write_acp_subprocess_test_config(&root);

    let acp_binary = build_acp_binary();
    tokio::task::LocalSet::new()
        .run_until(async {
            let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                .await
                .expect("bootstrap ACP client subprocess for capability coverage");
            let new_session = runtime
                .new_session(root.display().to_string())
                .await
                .expect("create ACP session for capability coverage");

            let file_path = root.join("notes").join("todo.txt");
            runtime
                .write_text_file_via_acp_for_tests(
                    new_session.session_id.clone(),
                    &file_path,
                    "alpha\nbeta\ngamma\n",
                )
                .await
                .expect("write text file through live ACP client filesystem capability");
            let slice = runtime
                .read_text_file_via_acp_for_tests(
                    new_session.session_id.clone(),
                    &file_path,
                    Some(2),
                    Some(1),
                )
                .await
                .expect("read text file through live ACP client filesystem capability");
            assert_eq!(slice, "beta\n");

            let terminal = runtime
                .run_terminal_command_via_acp_for_tests(
                    new_session.session_id.clone(),
                    "/bin/sh",
                    vec![
                        "-c".to_string(),
                        "pwd && [ -f marker.txt ] && printf 'ok\\n'".to_string(),
                    ],
                    None,
                    None,
                )
                .await
                .expect("run terminal command through live ACP client terminal capability");
            assert_eq!(terminal.exit_code, Some(0));
            assert!(!terminal.truncated);
            assert!(
                terminal.output.contains(&root.display().to_string()),
                "expected terminal output to include session cwd `{}`, got: {:?}",
                root.display(),
                terminal.output
            );
            assert!(
                terminal.output.contains("ok"),
                "expected terminal output to include success marker, got: {:?}",
                terminal.output
            );

            runtime
                .shutdown()
                .await
                .expect("shutdown ACP subprocess after filesystem/terminal test");
        })
        .await;

    cleanup(root);
}

#[test]
fn filesystem_capability_reads_within_session_cwd() {
    let root = unique_test_dir();
    fs::create_dir_all(root.join("notes")).expect("create session cwd");

    let session_id = acp::SessionId::new("session-fs-read");
    let file_path = root.join("notes").join("todo.txt");
    let mut filesystem = AcpFilesystemService::new();
    filesystem
        .register_session_cwd(session_id.clone(), &root)
        .expect("register session cwd");

    let capabilities = filesystem.client_capabilities();
    assert!(capabilities.fs.read_text_file);
    assert!(capabilities.fs.write_text_file);

    filesystem
        .write_text_file(acp::WriteTextFileRequest::new(
            session_id.clone(),
            &file_path,
            "alpha\nbeta\ngamma\n",
        ))
        .expect("write text file within session cwd");

    let response = filesystem
        .read_text_file(
            acp::ReadTextFileRequest::new(session_id, &file_path)
                .line(2)
                .limit(1),
        )
        .expect("read text file within session cwd");

    assert_eq!(response.content, "beta\n");
    cleanup(root);
}

#[test]
fn filesystem_capability_rejects_path_escape() {
    let root = unique_test_dir();
    let outside_root = unique_test_dir();
    fs::create_dir_all(&root).expect("create session cwd");
    fs::create_dir_all(&outside_root).expect("create outside root");

    let session_id = acp::SessionId::new("session-fs-escape");
    let escaped_path = outside_root.join("secret.txt");
    fs::write(&escaped_path, "outside").expect("write outside file");

    let mut filesystem = AcpFilesystemService::new();
    filesystem
        .register_session_cwd(session_id.clone(), &root)
        .expect("register session cwd");

    let read_error = filesystem
        .read_text_file(acp::ReadTextFileRequest::new(
            session_id.clone(),
            &escaped_path,
        ))
        .expect_err("read outside session cwd should fail cleanly");
    assert_eq!(read_error.code, acp::ErrorCode::InvalidParams);
    assert!(
        read_error
            .data
            .as_ref()
            .and_then(|data| data.get("message"))
            .and_then(|message| message.as_str())
            .is_some_and(|message| message.contains("escapes session cwd")),
        "expected cwd escape detail in read error, got: {read_error:?}"
    );

    let write_error = filesystem
        .write_text_file(acp::WriteTextFileRequest::new(
            session_id,
            outside_root.join("new.txt"),
            "still outside",
        ))
        .expect_err("write outside session cwd should fail cleanly");
    assert_eq!(write_error.code, acp::ErrorCode::InvalidParams);

    cleanup(root);
    cleanup(outside_root);
}

#[tokio::test]
async fn terminal_capability_executes_command_in_session_cwd() {
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create terminal session cwd");
    fs::write(root.join("marker.txt"), "marker").expect("write session cwd marker file");

    tokio::task::LocalSet::new()
        .run_until(async {
            let session_id = acp::SessionId::new("session-terminal-cwd");
            let terminal = AcpTerminalService::new();
            terminal
                .register_session_cwd(session_id.clone(), &root)
                .expect("register terminal session cwd");

            let capabilities = terminal.client_capabilities();
            assert!(capabilities.terminal);

            let created = terminal
                .create_terminal(
                    acp::CreateTerminalRequest::new("session-terminal-cwd", "/bin/sh").args(vec![
                        "-c".to_string(),
                        "pwd && [ -f marker.txt ] && printf 'ok\\n'".to_string(),
                    ]),
                )
                .await
                .expect("create terminal within negotiated session cwd");

            let exit = terminal
                .wait_for_terminal_exit(acp::WaitForTerminalExitRequest::new(
                    session_id.clone(),
                    created.terminal_id.clone(),
                ))
                .await
                .expect("wait for terminal command to finish");
            assert_eq!(exit.exit_status.exit_code, Some(0));

            let output = terminal
                .terminal_output(acp::TerminalOutputRequest::new(
                    session_id.clone(),
                    created.terminal_id.clone(),
                ))
                .await
                .expect("read terminal output after exit");
            assert!(
                output.output.contains(&root.display().to_string()),
                "expected terminal output to include session cwd `{}`, got: {:?}",
                root.display(),
                output.output
            );
            assert!(
                output.output.contains("ok"),
                "expected terminal output to include success marker, got: {:?}",
                output.output
            );
            assert_eq!(output.exit_status, Some(exit.exit_status.clone()));

            terminal
                .release_terminal(acp::ReleaseTerminalRequest::new(
                    session_id,
                    created.terminal_id,
                ))
                .await
                .expect("release completed terminal");
        })
        .await;

    cleanup(root);
}

#[tokio::test]
async fn terminal_capability_reports_spawn_failure() {
    let root = unique_test_dir();
    fs::create_dir_all(&root).expect("create spawn-failure session cwd");
    let missing_binary = root.join(format!(
        "missing-terminal-command{}",
        std::env::consts::EXE_SUFFIX
    ));

    tokio::task::LocalSet::new()
        .run_until(async {
            let session_id = acp::SessionId::new("session-terminal-spawn-failure");
            let terminal = AcpTerminalService::new();
            terminal
                .register_session_cwd(session_id.clone(), &root)
                .expect("register terminal session cwd for spawn failure");

            let error = terminal
                .create_terminal(acp::CreateTerminalRequest::new(
                    session_id,
                    missing_binary.display().to_string(),
                ))
                .await
                .expect_err(
                    "missing terminal command should return a recoverable ACP client error",
                );

            assert_eq!(error.code, acp::ErrorCode::InternalError);
            assert!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("message"))
                    .and_then(|message| message.as_str())
                    .is_some_and(|message| {
                        message.contains("failed to spawn terminal command")
                            && message.contains(&missing_binary.display().to_string())
                    }),
                "expected structured spawn failure detail, got: {error:?}"
            );
        })
        .await;

    cleanup(root);
}

fn startup_subprocess_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn build_acp_binary() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest_path = workspace_root().join("Cargo.toml");
    let output = Command::new(cargo)
        .arg("build")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("-p")
        .arg("fluent-code")
        .arg("--bin")
        .arg("fluent-code-acp")
        .output()
        .expect("spawn cargo build for fluent-code-acp");

    assert!(
        output.status.success(),
        "expected fluent-code-acp build to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let binary = acp_binary_path();
    assert!(
        binary.exists(),
        "expected fluent-code-acp binary at {}",
        binary.display()
    );
    binary
}

async fn wait_for_pending_permission(
    runtime: &fluent_code_tui::AcpClientRuntime,
) -> fluent_code_tui::PendingPermissionProjection {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = runtime.projection_snapshot().await;
        if let Some(permission) = snapshot.pending_permission {
            return permission;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for ACP permission request to reach the TUI projection"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_agent_transcript_content(
    runtime: &fluent_code_tui::AcpClientRuntime,
    needle: &str,
) -> fluent_code_tui::TuiProjectionState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = runtime.projection_snapshot().await;
        let combined_agent_text = snapshot
            .transcript_rows
            .iter()
            .filter(|row| row.source == TranscriptSource::Agent)
            .map(|row| row.content.as_str())
            .collect::<String>();
        if combined_agent_text.contains(needle) {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for an ACP agent transcript chunk containing `{needle}`"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_in_flight_agent_transcript_content(
    runtime: &fluent_code_tui::AcpClientRuntime,
) -> fluent_code_tui::TuiProjectionState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = runtime.projection_snapshot().await;
        let agent_rows = snapshot
            .transcript_rows
            .iter()
            .filter(|row| row.source == TranscriptSource::Agent)
            .collect::<Vec<_>>();
        if !agent_rows.is_empty() {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for in-flight ACP transcript content"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_tool_status(
    runtime: &fluent_code_tui::AcpClientRuntime,
    expected_status: &str,
) -> fluent_code_tui::TuiProjectionState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = runtime.projection_snapshot().await;
        if snapshot
            .tool_statuses
            .iter()
            .any(|status| status.status == expected_status)
        {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for ACP tool status `{expected_status}`"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_projected_session(
    runtime: &fluent_code_tui::AcpClientRuntime,
    session_id: &str,
) -> fluent_code_tui::TuiProjectionState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = runtime.projection_snapshot().await;
        if snapshot.session.session_id.as_deref() == Some(session_id) {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for ACP load replay to project session `{session_id}`"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn acp_binary_path() -> PathBuf {
    cargo_target_dir()
        .join("debug")
        .join(format!("fluent-code-acp{}", std::env::consts::EXE_SUFFIX))
}

fn cargo_target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root().join(path)
            }
        })
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn write_acp_subprocess_test_config(root: &Path) {
    fs::write(
        root.join("fluent-code.toml"),
        r#"data_dir = ".fluent-code"

[logging.file]
enabled = false

[logging.stderr]
enabled = false

[plugins]
enable_project_plugins = false
enable_global_plugins = false
project_dir = "plugins/project"
global_dir = "plugins/global"
"#,
    )
    .expect("write ACP subprocess test config");
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();

    std::env::temp_dir().join(format!(
        "fluent-code-tui-startup-test-{nanos}-{}",
        uuid::Uuid::new_v4()
    ))
}

fn cleanup(path: PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}

struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::current_dir().expect("capture current dir before startup test");
        std::env::set_current_dir(path).expect("switch current dir for startup test");
        Self { previous }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).expect("restore current dir after startup test");
    }
}

fn interrupted_delegation_fixture() -> StartupRecoveryFixture {
    let mut session = Session::new("startup recovery");
    let parent_run_id = uuid::Uuid::new_v4();
    let child_run_id = uuid::Uuid::new_v4();
    let task_invocation_id = uuid::Uuid::new_v4();
    let user_turn_id = uuid::Uuid::new_v4();
    let preceding_turn_id = uuid::Uuid::new_v4();

    let parent_run_sequence = session.allocate_replay_sequence();
    session.runs.push(RunRecord {
        id: parent_run_id,
        status: RunStatus::InProgress,
        parent_run_id: None,
        parent_tool_invocation_id: None,
        created_sequence: parent_run_sequence,
        terminal_sequence: None,
        terminal_stop_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    });
    let child_run_sequence = session.allocate_replay_sequence();
    session.runs.push(RunRecord {
        id: child_run_id,
        status: RunStatus::InProgress,
        parent_run_id: Some(parent_run_id),
        parent_tool_invocation_id: Some(task_invocation_id),
        created_sequence: child_run_sequence,
        terminal_sequence: None,
        terminal_stop_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    });
    let user_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: user_turn_id,
        run_id: parent_run_id,
        role: Role::User,
        content: "delegate work".to_string(),
        reasoning: String::new(),
        sequence_number: user_sequence_number,
        timestamp: Utc::now(),
    });
    let assistant_sequence_number = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: preceding_turn_id,
        run_id: parent_run_id,
        role: Role::Assistant,
        content: "I will delegate that task.".to_string(),
        reasoning: String::new(),
        sequence_number: assistant_sequence_number,
        timestamp: Utc::now(),
    });
    let child_prompt_sequence = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: uuid::Uuid::new_v4(),
        run_id: child_run_id,
        role: Role::User,
        content: "Inspect startup recovery".to_string(),
        reasoning: String::new(),
        sequence_number: child_prompt_sequence,
        timestamp: Utc::now(),
    });
    let child_output_sequence = session.allocate_replay_sequence();
    session.turns.push(Turn {
        id: uuid::Uuid::new_v4(),
        run_id: child_run_id,
        role: Role::Assistant,
        content: "Partial child output that should not be summarized".to_string(),
        reasoning: String::new(),
        sequence_number: child_output_sequence,
        timestamp: Utc::now(),
    });
    let invocation_sequence = session.allocate_replay_sequence();
    session.tool_invocations.push(ToolInvocationRecord {
        id: task_invocation_id,
        run_id: parent_run_id,
        tool_call_id: "task-call-1".to_string(),
        tool_name: "task".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: serde_json::json!({
            "agent": "explore",
            "prompt": "Inspect startup recovery"
        }),
        preceding_turn_id: Some(preceding_turn_id),
        approval_state: ToolApprovalState::Approved,
        execution_state: ToolExecutionState::Running,
        result: None,
        error: None,
        delegation: Some(TaskDelegationRecord {
            child_run_id: Some(child_run_id),
            agent_name: Some("explore".to_string()),
            prompt: Some("Inspect startup recovery".to_string()),
            status: TaskDelegationStatus::Running,
        }),
        sequence_number: invocation_sequence,
        requested_at: Utc::now(),
        approved_at: Some(Utc::now()),
        completed_at: None,
    });

    StartupRecoveryFixture {
        session,
        child_run_id,
    }
}
