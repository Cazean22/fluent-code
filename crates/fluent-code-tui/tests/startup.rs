use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_client_protocol as acp;
use chrono::{Duration as ChronoDuration, Utc};
use fluent_code_app::app::RESTART_INTERRUPTED_TASK_RESULT;
use fluent_code_app::error::FluentCodeError;
use fluent_code_app::session::model::{
    Role, RunRecord, RunStatus, Session, TaskDelegationRecord, TaskDelegationStatus,
    ToolApprovalState, ToolExecutionState, ToolInvocationRecord, ToolSource, TranscriptItemRecord,
    Turn,
};
use fluent_code_app::session::store::{FsSessionStore, SessionStore};
use fluent_code_tui::{
    AcpFilesystemService, AcpLaunchOptions, AcpTerminalService, ProjectionActivitySnapshot,
    SubprocessStatus, TranscriptSource, bootstrap_client_for_tests,
    initialize_default_session_for_tests, run_with_terminal_hooks_for_tests,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::timeout;

#[allow(dead_code)]
#[path = "../src/acp.rs"]
mod acp_projection_regression;
#[allow(dead_code)]
#[path = "../src/conversation.rs"]
mod conversation;
#[allow(dead_code)]
#[path = "../src/markdown_render.rs"]
mod markdown_render;
#[allow(dead_code)]
#[path = "../src/terminal.rs"]
mod terminal;
#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;
#[allow(dead_code)]
#[path = "../src/ui_state.rs"]
mod ui_state;
#[allow(dead_code)]
#[path = "../src/view.rs"]
mod view;

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

#[test]
fn session_browser_uses_backend_order_and_loading_session_preserves_projection_order() {
    let runtime = tokio::runtime::Runtime::new().expect("test tokio runtime");
    runtime.block_on(async {
        let _guard = startup_subprocess_test_lock().lock().await;
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create session browser root");
        write_acp_subprocess_test_config(&root);

        let store = FsSessionStore::new(root.join(".fluent-code"));
        let older_session = ordered_session_fixture(
            "Older session",
            Utc::now() - ChronoDuration::hours(1),
            "inspect repo",
            "PersistSession",
            "README.md",
            "older answer",
        );
        let newest_session = ordered_session_fixture(
            "Newest session",
            Utc::now(),
            "latest prompt",
            "ProjectionController",
            "src/lib.rs",
            "latest answer",
        );
        store.create(&older_session).expect("persist older session fixture");
        store
            .create(&newest_session)
            .expect("persist newest session fixture");

        let acp_binary = build_acp_binary();
        {
            let _cwd_guard = CurrentDirGuard::set(&root);
            tokio::task::LocalSet::new()
                .run_until(async {
                    let runtime =
                        bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                            .await
                            .expect("bootstrap ACP client subprocess for session browser test");
                    initialize_default_session_for_tests(&runtime, &root)
                        .await
                        .expect("initialize default ACP session for session browser test");

                    let initial_snapshot = wait_for_projection_match(
                        &runtime,
                        "the default ACP session browser snapshot",
                        |snapshot| {
                            snapshot.session.session_id.as_deref()
                                == Some(newest_session.id.to_string().as_str())
                                && snapshot.sessions.len() == 2
                        },
                    )
                    .await;
                    assert_eq!(
                        initial_snapshot
                            .sessions
                            .iter()
                            .map(|session| session.title.as_deref())
                            .collect::<Vec<_>>(),
                        vec![Some("Newest session"), Some("Older session")]
                    );

                    let initial_render =
                        fluent_code_tui::render_projection_frame_text_for_tests(&initial_snapshot);
                    assert!(
                        initial_render.contains(&format!(
                            "› Newest session · {}",
                            abbreviated_session_id(&newest_session.id.to_string())
                        )),
                        "expected the session browser to mark the default ACP session as current, got:\n{initial_render}"
                    );

                    runtime
                        .load_session(older_session.id.to_string(), &root)
                        .await
                        .expect("load older ACP session through the session browser path");
                    let older_snapshot = wait_for_projected_session(&runtime, &older_session.id.to_string()).await;
                    assert_eq!(
                        older_snapshot
                            .sessions
                            .iter()
                            .map(|session| session.title.as_deref())
                            .collect::<Vec<_>>(),
                        vec![Some("Newest session"), Some("Older session")]
                    );

                    let older_render =
                        fluent_code_tui::render_projection_frame_text_for_tests(&older_snapshot);
                    let newest_browser_line = format!(
                        "  Newest session · {}",
                        abbreviated_session_id(&newest_session.id.to_string())
                    );
                    let older_browser_line = format!(
                        "› Older session · {}",
                        abbreviated_session_id(&older_session.id.to_string())
                    );
                    assert!(
                        older_render.contains(&newest_browser_line)
                            && older_render.contains(&older_browser_line),
                        "expected the session browser to keep backend newest-first order while marking the loaded session, got:\n{older_render}"
                    );

                    let prompt_index = older_render
                        .find("inspect repo")
                        .expect("loaded session should render the older user turn");
                    let search_index = older_render
                        .find(&format!(
                            "tool_search-{} PersistSession",
                            abbreviated_session_id(&older_session.id.to_string())
                        ))
                        .expect("loaded session should render the older search tool in place");
                    let read_index = older_render
                        .find(&format!(
                            "tool_read-{} README.md",
                            abbreviated_session_id(&older_session.id.to_string())
                        ))
                        .expect("loaded session should render the older read tool in place");
                    let answer_index = older_render
                        .find("older answer")
                        .expect("loaded session should render the older assistant turn");
                    assert!(
                        prompt_index < search_index
                            && search_index < read_index
                            && read_index < answer_index,
                        "expected the loaded ACP session transcript/tool chronology to stay in replay order, got:\n{older_render}"
                    );

                    runtime
                        .shutdown()
                        .await
                        .expect("shutdown ACP subprocess after session browser test");
                })
                .await;
        }

        cleanup(root);
    });
}

#[test]
fn startup_load_tolerates_legacy_session_browser_responses_missing_cwd() {
    let runtime = tokio::runtime::Runtime::new().expect("test tokio runtime");
    runtime.block_on(async {
        let _guard = startup_subprocess_test_lock().lock().await;
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create legacy session browser root");
        write_acp_subprocess_test_config(&root);

        let store = FsSessionStore::new(root.join(".fluent-code"));
        let existing = store
            .create_new_session()
            .expect("create latest session for legacy browser compatibility");
        let legacy_acp_binary = write_legacy_session_list_acp_binary(&root, &existing.id.to_string());

        {
            let _cwd_guard = CurrentDirGuard::set(&root);
            tokio::task::LocalSet::new()
                .run_until(async {
                    let runtime = bootstrap_client_for_tests(AcpLaunchOptions::new(
                        &legacy_acp_binary,
                        &root,
                    ))
                    .await
                    .expect("bootstrap legacy ACP client subprocess for startup load");

                    initialize_default_session_for_tests(&runtime, &root)
                        .await
                        .expect("initialize default ACP startup session despite legacy browser response");

                    let startup_snapshot = runtime.projection_snapshot().await;
                    assert_eq!(
                        startup_snapshot.session.session_id.as_deref(),
                        Some(existing.id.to_string().as_str())
                    );
                    assert!(startup_snapshot.startup_error.is_none());
                    assert!(
                        startup_snapshot.sessions.is_empty(),
                        "expected the legacy session/list decode failure to leave the browser unchanged, got: {startup_snapshot:?}"
                    );

                    runtime
                        .load_session(existing.id.to_string(), &root)
                        .await
                        .expect("reload ACP session despite legacy browser response");

                    let reload_snapshot = runtime.projection_snapshot().await;
                    assert_eq!(
                        reload_snapshot.session.session_id.as_deref(),
                        Some(existing.id.to_string().as_str())
                    );
                    assert!(reload_snapshot.startup_error.is_none());

                    runtime
                        .shutdown()
                        .await
                        .expect("shutdown legacy ACP subprocess after compatibility test");
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
                        .tool_statuses()
                        .iter()
                        .any(|status| status.tool_call_id == "task-call-1"),
                    "expected ACP replay to project the terminalized delegated tool, got: {snapshot:?}"
                );
                assert!(
                    !snapshot.transcript_rows().is_empty(),
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
                        .tool_statuses()
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
                    .tool_statuses()
                    .iter()
                    .any(|status| status.status == "completed"),
                "expected ACP tool status updates to include a completed tool"
            );
            assert!(
                !snapshot.transcript_rows().is_empty(),
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
    write_acp_subprocess_test_config_with_chunk_delay(&root, Some(75));

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
                let (prompt_result, streaming_snapshots) = tokio::join!(
                    runtime.prompt(
                        new_session.session_id.clone(),
                        "please stream a response over ACP",
                    ),
                    wait_for_monotonic_in_flight_agent_transcript_growth(&runtime, full_response)
                );
                assert_eq!(
                    streaming_snapshots.len(),
                    2,
                    "expected the helper to return two successive in-flight partial snapshots"
                );
                let first_partial = &streaming_snapshots[0];
                let second_partial = &streaming_snapshots[1];
                let first_partial_agent_rows = first_partial
                    .transcript_rows()
                    .into_iter()
                    .filter(|row| row.source == TranscriptSource::Agent)
                    .collect::<Vec<_>>();
                let second_partial_agent_rows = second_partial
                    .transcript_rows()
                    .into_iter()
                    .filter(|row| row.source == TranscriptSource::Agent)
                    .collect::<Vec<_>>();
                assert_eq!(
                    first_partial_agent_rows.len(),
                    1,
                    "expected the first in-flight ACP snapshot to keep one coalesced agent row, got: {first_partial:?}"
                );
                assert_eq!(
                    second_partial_agent_rows.len(),
                    1,
                    "expected the second in-flight ACP snapshot to keep one coalesced agent row, got: {second_partial:?}"
                );
                assert!(
                    !first_partial_agent_rows[0].content.is_empty(),
                    "expected the first in-flight ACP snapshot to contain partial streamed content, got: {first_partial:?}"
                );
                assert!(
                    second_partial_agent_rows[0].content.len()
                        > first_partial_agent_rows[0].content.len(),
                    "expected successive in-flight ACP snapshots to show monotonic transcript growth, got first={:?}, second={:?}",
                    first_partial_agent_rows[0].content,
                    second_partial_agent_rows[0].content
                );
                assert!(
                    !first_partial_agent_rows[0].content.contains(full_response)
                        && !second_partial_agent_rows[0].content.contains(full_response),
                    "expected both in-flight ACP snapshots to remain partial rather than final, got: {streaming_snapshots:?}"
                );

                let prompt_result = prompt_result
                    .expect("prompt flow to complete through the ACP subprocess");
                assert_eq!(prompt_result.stop_reason, agent_client_protocol::StopReason::EndTurn);

                let completed_snapshot = runtime.projection_snapshot().await;
                let completed_agent_rows = completed_snapshot
                    .transcript_rows()
                    .into_iter()
                    .filter(|row| row.source == TranscriptSource::Agent)
                    .collect::<Vec<_>>();
                assert!(
                    !completed_snapshot.prompt_in_flight,
                    "expected the prompt-finished projection snapshot to be available immediately after ACP completion, got: {completed_snapshot:?}"
                );
                assert_eq!(
                    completed_agent_rows.len(),
                    1,
                    "expected ACP prompt completion to leave one coalesced agent row immediately available, got: {completed_snapshot:?}"
                );
                assert!(
                    completed_agent_rows[0].content.contains(full_response),
                    "expected ACP prompt completion to flush the final streamed chunk into the projection immediately, got: {:?}",
                    completed_agent_rows[0].content
                );
            }

            let snapshot = wait_for_agent_transcript_content(&runtime, full_response).await;
            assert!(snapshot.pending_permission.is_none());
            let agent_rows = snapshot
                .transcript_rows()
                .into_iter()
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
async fn projection_loop_redraws_stream_updates_without_waiting_for_full_input_poll() {
    acp_projection_regression::assert_projection_loop_redraws_stream_updates_without_waiting_for_full_input_poll()
        .await
        .expect("projection loop redraw-order regression helper to pass");
}

#[tokio::test]
async fn projection_state_flushes_terminal_stream_updates_immediately() {
    acp_projection_regression::assert_projection_state_flushes_terminal_stream_updates_immediately(
    )
    .await
    .expect("projection terminal-state flush regression helper to pass");
}

#[tokio::test]
async fn projection_loop_batches_notifications_without_starving_input() {
    acp_projection_regression::assert_projection_loop_batches_notifications_without_starving_input(
    )
    .await
    .expect("projection burst-coalescing regression helper to pass");
}

#[tokio::test]
async fn projection_wait_path_stays_idle_until_activity_wake() {
    acp_projection_regression::assert_projection_wait_path_stays_idle_until_activity_wake()
        .await
        .expect("projection idle-wait regression helper to pass");
}

#[tokio::test]
async fn projection_wake_does_not_miss_activity_with_release_acquire_ordering() {
    acp_projection_regression::assert_projection_wake_does_not_miss_activity_with_release_acquire_ordering()
        .await
        .expect("projection wake ordering regression helper to pass");
}

#[tokio::test]
async fn projection_wait_for_activity_still_blocks_without_new_sequence() {
    acp_projection_regression::assert_projection_wait_for_activity_still_blocks_without_new_sequence()
        .await
        .expect("projection wait ordering regression helper to pass");
}

#[tokio::test]
async fn projection_loop_flushes_terminal_update_before_queued_quit_under_burst_activity() {
    acp_projection_regression::assert_projection_loop_flushes_terminal_update_before_queued_quit_under_burst_activity()
        .await
        .expect("projection burst flush regression helper to pass");
}

#[test]
fn render_contract_distinguishes_committed_history_from_active_cell() {
    acp_projection_regression::assert_render_contract_distinguishes_committed_history_from_active_cell();
}

#[test]
fn conversation_projection_cache_preserves_history_output() {
    acp_projection_regression::assert_conversation_projection_cache_preserves_history_output();
}

#[test]
fn conversation_projection_cache_invalidates_on_transcript_change() {
    acp_projection_regression::assert_conversation_projection_cache_invalidates_on_transcript_change();
}

#[test]
fn startup_restore_with_projection_cache_matches_uncached_output() {
    acp_projection_regression::assert_startup_restore_with_projection_cache_matches_uncached_output(
    );
}

#[test]
fn session_render_regression_completed_streaming_and_recovery() {
    acp_projection_regression::assert_session_render_regression_completed_and_streaming();

    let runtime = tokio::runtime::Runtime::new().expect("session render regression runtime");
    runtime.block_on(async {
        let _guard = startup_subprocess_test_lock().lock().await;
        let root = unique_test_dir();
        fs::create_dir_all(&root).expect("create session render regression root");
        write_acp_subprocess_test_config(&root);

        let store = FsSessionStore::new(root.join(".fluent-code"));
        let fixture = interrupted_delegation_fixture();
        store
            .create(&fixture.session)
            .expect("persist ACP replay frame regression fixture");

        let acp_binary = build_acp_binary();
        {
            let _cwd_guard = CurrentDirGuard::set(&root);
            tokio::task::LocalSet::new()
                .run_until(async {
                    let runtime =
                        bootstrap_client_for_tests(AcpLaunchOptions::new(&acp_binary, &root))
                            .await
                            .expect("bootstrap ACP client subprocess for frame replay regression");
                    initialize_default_session_for_tests(&runtime, &root)
                        .await
                        .expect("initialize default ACP session for frame replay regression");

                    let snapshot =
                        wait_for_projected_session(&runtime, &fixture.session.id.to_string()).await;
                    assert!(snapshot.prompt_in_flight);
                    assert!(
                        snapshot
                            .tool_statuses()
                            .iter()
                            .any(|status| status.status == "completed"),
                        "expected recovered ACP replay to retain the synthesized completed task tool status, got: {snapshot:?}"
                    );

                    let mut rendered_snapshot = snapshot.clone();
                    rendered_snapshot.session.title = None;
                    rendered_snapshot.subprocess.status = SubprocessStatus::Initialized {
                        binary_path: PathBuf::from("fluent-code-acp"),
                        pid: 7,
                        protocol_version: "1".to_string(),
                    };

                    let rendered =
                        fluent_code_tui::render_projection_frame_text_for_tests(&rendered_snapshot);
                    let recovery_body = vec![
                        String::new(),
                        "you".to_string(),
                        "delegate work".to_string(),
                        String::new(),
                        String::new(),
                        "you".to_string(),
                        "Inspect startup recovery".to_string(),
                        String::new(),
                        String::new(),
                        "  ⏵ tool_task-call-1 · approved / completed".to_string(),
                        "    tool_task-call-1".to_string(),
                        String::new(),
                        "assistant".to_string(),
                        "I will delegate that task.".to_string(),
                        String::new(),
                        String::new(),
                        "assistant".to_string(),
                        "Partial child output that should not be summarized".to_string(),
                        String::new(),
                        String::new(),
                        "  ● running".to_string(),
                    ];
                    let recovery_body_refs =
                        recovery_body.iter().map(String::as_str).collect::<Vec<_>>();
                    let recovery_session_browser = [format!(
                        "› startup recovery · {}",
                        abbreviated_session_id(&fixture.session.id.to_string())
                    )];
                    let recovery_session_browser_refs = recovery_session_browser
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>();
                    let expected = fluent_code_tui::expected_projection_frame_text_for_tests(
                        " fluent-code │ acp connected",
                        &recovery_session_browser_refs,
                        &recovery_body_refs,
                        "",
                        "Prompt running through ACP. Esc/Ctrl-C cancels the active turn.",
                    );
                    assert_eq!(rendered, expected);

                    runtime
                        .shutdown()
                        .await
                        .expect("shutdown ACP subprocess after frame replay regression");
                })
                .await;
        }

        cleanup(root);
    });
}

#[test]
fn session_render_regression_permission_delegation_and_legacy_fidelity() {
    acp_projection_regression::assert_session_render_regression_permission_delegation_and_legacy();
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
                snapshot.tool_statuses().iter().any(|status| status.status == "completed"),
                "expected ACP tool lifecycle projection to include a completed tool, got: {snapshot:?}"
            );
            let combined_agent_text = snapshot
                .transcript_rows()
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
    wait_for_projection_match(
        runtime,
        "an ACP permission request to reach the TUI projection",
        |snapshot| snapshot.pending_permission.is_some(),
    )
    .await
    .pending_permission
    .expect("wait helper should only return after a permission is projected")
}

async fn wait_for_agent_transcript_content(
    runtime: &fluent_code_tui::AcpClientRuntime,
    needle: &str,
) -> fluent_code_tui::TuiProjectionState {
    wait_for_projection_match(
        runtime,
        &format!("an ACP agent transcript chunk containing `{needle}`"),
        |snapshot| {
            snapshot
                .transcript_rows()
                .into_iter()
                .filter(|row| row.source == TranscriptSource::Agent)
                .map(|row| row.content)
                .collect::<String>()
                .contains(needle)
        },
    )
    .await
}

async fn wait_for_monotonic_in_flight_agent_transcript_growth(
    runtime: &fluent_code_tui::AcpClientRuntime,
    full_response: &str,
) -> Vec<fluent_code_tui::TuiProjectionState> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut snapshots = Vec::new();
    let mut last_content = String::new();
    let mut snapshot = runtime.projection_activity_snapshot_for_tests().await;

    loop {
        let agent_rows = snapshot
            .projection
            .transcript_rows()
            .into_iter()
            .filter(|row| row.source == TranscriptSource::Agent)
            .collect::<Vec<_>>();

        if agent_rows.len() == 1 {
            let content = agent_rows[0].content.as_str();
            if !content.is_empty() && !content.contains(full_response) {
                if !last_content.is_empty() {
                    assert!(
                        content.len() >= last_content.len(),
                        "expected in-flight ACP transcript content to grow monotonically, got previous={last_content:?}, current={content:?}"
                    );
                }

                if content.len() > last_content.len() {
                    snapshots.push(snapshot.projection.clone());
                    last_content = content.to_string();
                    if snapshots.len() == 2 {
                        return snapshots;
                    }
                }
            }
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for successive in-flight ACP transcript growth before completion"
        );

        snapshot = wait_for_projection_activity(runtime, snapshot, deadline, || {
            "successive in-flight ACP transcript growth before completion".to_string()
        })
        .await;
    }
}

async fn wait_for_tool_status(
    runtime: &fluent_code_tui::AcpClientRuntime,
    expected_status: &str,
) -> fluent_code_tui::TuiProjectionState {
    wait_for_projection_match(
        runtime,
        &format!("ACP tool status `{expected_status}`"),
        |snapshot| {
            snapshot
                .tool_statuses()
                .iter()
                .any(|status| status.status == expected_status)
        },
    )
    .await
}

async fn wait_for_projected_session(
    runtime: &fluent_code_tui::AcpClientRuntime,
    session_id: &str,
) -> fluent_code_tui::TuiProjectionState {
    wait_for_projection_match(
        runtime,
        &format!("ACP load replay to project session `{session_id}`"),
        |snapshot| snapshot.session.session_id.as_deref() == Some(session_id),
    )
    .await
}

async fn wait_for_projection_match<F>(
    runtime: &fluent_code_tui::AcpClientRuntime,
    description: &str,
    predicate: F,
) -> fluent_code_tui::TuiProjectionState
where
    F: Fn(&fluent_code_tui::TuiProjectionState) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut snapshot = runtime.projection_activity_snapshot_for_tests().await;

    loop {
        if predicate(&snapshot.projection) {
            return snapshot.projection;
        }

        snapshot =
            wait_for_projection_activity(runtime, snapshot, deadline, || description.to_string())
                .await;
    }
}

async fn wait_for_projection_activity<Describe>(
    runtime: &fluent_code_tui::AcpClientRuntime,
    snapshot: ProjectionActivitySnapshot,
    deadline: tokio::time::Instant,
    describe: Describe,
) -> ProjectionActivitySnapshot
where
    Describe: FnOnce() -> String,
{
    let description = describe();
    let remaining = deadline
        .checked_duration_since(tokio::time::Instant::now())
        .unwrap_or_else(|| panic!("timed out waiting for {description}"));

    timeout(
        remaining,
        runtime.wait_for_projection_activity_for_tests(snapshot.activity_sequence),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
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
    write_acp_subprocess_test_config_with_chunk_delay(root, None);
}

fn write_legacy_session_list_acp_binary(root: &Path, session_id: &str) -> PathBuf {
    let binary_path = root.join("legacy-session-list-acp.py");
    fs::write(
        &binary_path,
        format!(
            r#"#!/usr/bin/env python3
import json
import sys

session_id = {session_id:?}

for line in sys.stdin:
    message = json.loads(line)
    request_id = message.get("id")
    method = message.get("method")

    if method == "initialize":
        response = {{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{
                "protocolVersion": "1",
                "agentCapabilities": {{
                    "loadSession": True,
                    "sessionCapabilities": {{"list": {{}}}}
                }},
                "agentInfo": {{
                    "name": "legacy-session-list-acp",
                    "version": "0.0.0"
                }}
            }}
        }}
    elif method == "session/load":
        params = message.get("params") or {{}}
        session_id = params.get("sessionId", session_id)
        response = {{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{}}
        }}
    elif method == "session/list":
        response = {{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{
                "sessions": [{{
                    "sessionId": session_id,
                    "title": "Legacy browser session",
                    "updatedAt": "2026-04-05T00:00:00Z"
                }}]
            }}
        }}
    else:
        response = {{
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {{
                "code": -32601,
                "message": "Method not found"
            }}
        }}

    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
            session_id = session_id,
        ),
    )
    .expect("write legacy ACP session/list test binary");
    let mut permissions = fs::metadata(&binary_path)
        .expect("read legacy ACP session/list test binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary_path, permissions)
        .expect("make legacy ACP session/list test binary executable");
    binary_path
}

fn write_acp_subprocess_test_config_with_chunk_delay(root: &Path, chunk_delay_ms: Option<u64>) {
    let mock_provider_config = chunk_delay_ms.map_or_else(String::new, |chunk_delay_ms| {
        format!("\n[model_providers.mock]\nchunk_delay_ms = {chunk_delay_ms}\n")
    });
    fs::write(
        root.join("fluent-code.toml"),
        format!(
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
{mock_provider_config}"#
        ),
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

fn abbreviated_session_id(session_id: &str) -> &str {
    session_id.split('-').next().unwrap_or(session_id)
}

fn ordered_session_fixture(
    title: &str,
    updated_at: chrono::DateTime<Utc>,
    user_prompt: &str,
    search_query: &str,
    read_path: &str,
    assistant_answer: &str,
) -> Session {
    let run_id = uuid::Uuid::new_v4();
    let mut session = Session::new(title);
    session.updated_at = updated_at;

    let user_turn = Turn {
        id: uuid::Uuid::new_v4(),
        run_id,
        role: Role::User,
        content: user_prompt.to_string(),
        reasoning: String::new(),
        sequence_number: 1,
        timestamp: updated_at + ChronoDuration::minutes(10),
    };
    let assistant_turn = Turn {
        id: uuid::Uuid::new_v4(),
        run_id,
        role: Role::Assistant,
        content: assistant_answer.to_string(),
        reasoning: String::new(),
        sequence_number: 4,
        timestamp: updated_at - ChronoDuration::minutes(5),
    };
    let search_tool = ToolInvocationRecord {
        id: uuid::Uuid::new_v4(),
        run_id,
        tool_call_id: format!("search-{}", abbreviated_session_id(&session.id.to_string())),
        tool_name: "search".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: json!({"query": search_query}),
        preceding_turn_id: Some(user_turn.id),
        approval_state: ToolApprovalState::Approved,
        execution_state: ToolExecutionState::Completed,
        result: Some("search result".to_string()),
        error: None,
        delegation: None,
        sequence_number: 2,
        requested_at: updated_at - ChronoDuration::minutes(20),
        approved_at: Some(updated_at - ChronoDuration::minutes(19)),
        completed_at: Some(updated_at - ChronoDuration::minutes(18)),
    };
    let read_tool = ToolInvocationRecord {
        id: uuid::Uuid::new_v4(),
        run_id,
        tool_call_id: format!("read-{}", abbreviated_session_id(&session.id.to_string())),
        tool_name: "read".to_string(),
        tool_source: ToolSource::BuiltIn,
        arguments: json!({"path": read_path}),
        preceding_turn_id: Some(user_turn.id),
        approval_state: ToolApprovalState::Approved,
        execution_state: ToolExecutionState::Completed,
        result: Some("read result".to_string()),
        error: None,
        delegation: None,
        sequence_number: 3,
        requested_at: updated_at - ChronoDuration::minutes(30),
        approved_at: Some(updated_at - ChronoDuration::minutes(29)),
        completed_at: Some(updated_at - ChronoDuration::minutes(28)),
    };

    session.turns = vec![user_turn.clone(), assistant_turn.clone()];
    session.tool_invocations = vec![read_tool.clone(), search_tool.clone()];
    session.transcript_items = vec![
        TranscriptItemRecord::from_turn(&user_turn),
        TranscriptItemRecord::from_tool_invocation(&search_tool),
        TranscriptItemRecord::from_tool_invocation(&read_tool),
        TranscriptItemRecord::from_turn(&assistant_turn),
    ];
    session.upsert_run(run_id, RunStatus::Completed);
    session
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
    session.rebuild_run_indexes();

    StartupRecoveryFixture {
        session,
        child_run_id,
    }
}
