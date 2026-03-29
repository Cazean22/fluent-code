use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use fluent_code_app::app::{AppState, AppStatus, Msg, RESTART_INTERRUPTED_TASK_RESULT};
use fluent_code_app::bootstrap::BootstrapContext;
use fluent_code_app::config::{
    AcpConfig, AcpSessionDefaultsConfig, Config, LoggingConfig, LoggingFileConfig,
    LoggingStderrConfig, ModelConfig, PluginConfig,
};
use fluent_code_app::error::FluentCodeError;
use fluent_code_app::runtime::Runtime;
use fluent_code_app::session::model::{
    Role, RunRecord, RunStatus, Session, TaskDelegationRecord, TaskDelegationStatus,
    ToolApprovalState, ToolExecutionState, ToolInvocationRecord, ToolSource, Turn,
};
use fluent_code_app::session::store::{FsSessionStore, SessionStore};
use fluent_code_provider::{MockProvider, ProviderClient};
use fluent_code_tui::{
    TuiStartup, recover_startup_state_for_tests, run_startup_with_terminal_hooks_for_tests,
};
use ratatui::{Terminal, backend::CrosstermBackend};

struct StartupRecoveryFixture {
    session: Session,
    parent_run_id: uuid::Uuid,
    child_run_id: uuid::Uuid,
}

#[test]
fn startup_uses_latest_session_by_default() {
    let root = unique_test_dir();
    let store = FsSessionStore::new(root.join(".fluent-code"));
    let existing = store.create_new_session().expect("create latest session");
    let bootstrap = BootstrapContext::from_config(test_config(&root)).expect("bootstrap context");

    let startup = TuiStartup::from_bootstrap(bootstrap).expect("prepare startup from bootstrap");

    assert_eq!(startup.session.id, existing.id);
    cleanup(root);
}

#[tokio::test]
async fn restores_terminal_when_startup_recovery_fails() {
    let root = unique_test_dir();
    let bootstrap = BootstrapContext::from_config(test_config(&root)).expect("bootstrap context");
    let startup = TuiStartup::from_bootstrap(bootstrap).expect("prepare startup from bootstrap");
    let restored = AtomicBool::new(false);

    let err = run_startup_with_terminal_hooks_for_tests(
        startup,
        || {
            let backend = CrosstermBackend::new(std::io::stdout());
            Ok(Terminal::new(backend).expect("test terminal"))
        },
        |terminal| {
            restored.store(true, Ordering::SeqCst);
            drop(terminal);
            Ok(())
        },
        |_state, _store, _runtime, _sender| async {
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
    let root = unique_test_dir();
    let store = FsSessionStore::new(root.clone());
    let fixture = interrupted_delegation_fixture();
    store
        .create(&fixture.session)
        .expect("persist startup fixture");

    let mut state = AppState::new(fixture.session);
    let runtime = Runtime::new(ProviderClient::Mock(MockProvider::with_chunk_delay(
        Duration::from_millis(5),
    )));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    recover_startup_state_for_tests(&mut state, &store, &runtime, tx)
        .await
        .expect("recover interrupted delegated child on startup");

    assert_eq!(state.active_run_id, Some(fixture.parent_run_id));
    assert!(matches!(state.status, AppStatus::Generating));
    assert_eq!(
        state.session.tool_invocations[0].result.as_deref(),
        Some(RESTART_INTERRUPTED_TASK_RESULT)
    );
    assert_eq!(
        state.session.tool_invocations[0].delegation_status(),
        Some(TaskDelegationStatus::Failed)
    );
    assert!(matches!(
        state
            .session
            .find_run(fixture.child_run_id)
            .map(|run| run.status),
        Some(RunStatus::Failed)
    ));

    let persisted = store
        .load(&state.session.id)
        .expect("load recovered session");
    assert_eq!(
        persisted.tool_invocations[0].result.as_deref(),
        Some(RESTART_INTERRUPTED_TASK_RESULT)
    );
    assert!(matches!(
        persisted.runs.iter().find(|run| run.id == fixture.child_run_id),
        Some(run) if run.status == RunStatus::Failed
    ));

    let message = tokio::time::timeout(Duration::from_millis(300), rx.recv())
        .await
        .expect("receive resumed parent runtime message")
        .expect("runtime message after startup recovery");
    assert!(matches!(message, Msg::AssistantChunk { .. }));

    cleanup(root);
}

#[tokio::test]
async fn startup_recovery_fails_closed_for_malformed_lineage() {
    let root = unique_test_dir();
    let store = FsSessionStore::new(root.clone());
    let mut fixture = interrupted_delegation_fixture();
    fixture
        .session
        .runs
        .retain(|run| run.id != fixture.child_run_id);
    store
        .create(&fixture.session)
        .expect("persist malformed startup fixture");

    let mut state = AppState::new(fixture.session.clone());
    let runtime = Runtime::new(ProviderClient::Mock(MockProvider::new(None)));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    recover_startup_state_for_tests(&mut state, &store, &runtime, tx)
        .await
        .expect("startup recovery fail-closed handling");

    assert!(matches!(state.status, AppStatus::Error(_)));
    assert!(state.active_run_id.is_none());
    assert_eq!(
        state.session.tool_invocations[0].delegation_status(),
        Some(TaskDelegationStatus::Running)
    );
    assert!(!matches!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
        Ok(Some(_))
    ));

    let persisted = store
        .load(&state.session.id)
        .expect("reload malformed session");
    assert_eq!(
        persisted.tool_invocations[0].delegation_status(),
        Some(TaskDelegationStatus::Running)
    );

    cleanup(root);
}

fn test_config(root: &Path) -> Config {
    let data_dir = root.join(".fluent-code");

    Config {
        config_path: None,
        data_dir: data_dir.clone(),
        logging: LoggingConfig {
            file: LoggingFileConfig {
                enabled: false,
                path: data_dir.join("logs/fluent-code.log"),
                level: "info".to_string(),
            },
            stderr: LoggingStderrConfig {
                enabled: false,
                level: "info".to_string(),
            },
        },
        model: ModelConfig {
            provider: "mock".to_string(),
            model: "gpt-4.1-mini".to_string(),
            reasoning_effort: None,
            system_prompt: "You are a helpful coding assistant.".to_string(),
        },
        agents: None,
        plugins: PluginConfig {
            enable_project_plugins: false,
            enable_global_plugins: false,
            project_dir: root.join("plugins/project"),
            global_dir: root.join("plugins/global"),
        },
        acp: AcpConfig {
            protocol_version: 1,
            auth_methods: vec![],
            session_defaults: AcpSessionDefaultsConfig {
                system_prompt: "You are a helpful coding assistant.".to_string(),
                reasoning_effort: None,
            },
        },
        model_providers: HashMap::new(),
    }
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();

    std::env::temp_dir().join(format!("fluent-code-tui-startup-test-{nanos}"))
}

fn cleanup(path: PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}

fn interrupted_delegation_fixture() -> StartupRecoveryFixture {
    let mut session = Session::new("startup recovery");
    let parent_run_id = uuid::Uuid::new_v4();
    let child_run_id = uuid::Uuid::new_v4();
    let task_invocation_id = uuid::Uuid::new_v4();
    let user_turn_id = uuid::Uuid::new_v4();
    let preceding_turn_id = uuid::Uuid::new_v4();

    session.runs.push(RunRecord {
        id: parent_run_id,
        status: RunStatus::InProgress,
        parent_run_id: None,
        parent_tool_invocation_id: None,
        created_sequence: 1,
        terminal_sequence: None,
        terminal_stop_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    });
    session.runs.push(RunRecord {
        id: child_run_id,
        status: RunStatus::InProgress,
        parent_run_id: Some(parent_run_id),
        parent_tool_invocation_id: Some(task_invocation_id),
        created_sequence: 2,
        terminal_sequence: None,
        terminal_stop_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    });
    session.turns.push(Turn {
        id: user_turn_id,
        run_id: parent_run_id,
        role: Role::User,
        content: "delegate work".to_string(),
        reasoning: String::new(),
        sequence_number: 1,
        timestamp: Utc::now(),
    });
    session.turns.push(Turn {
        id: preceding_turn_id,
        run_id: parent_run_id,
        role: Role::Assistant,
        content: "I will delegate that task.".to_string(),
        reasoning: String::new(),
        sequence_number: 1,
        timestamp: Utc::now(),
    });
    session.turns.push(Turn {
        id: uuid::Uuid::new_v4(),
        run_id: child_run_id,
        role: Role::User,
        content: "Inspect startup recovery".to_string(),
        reasoning: String::new(),
        sequence_number: 1,
        timestamp: Utc::now(),
    });
    session.turns.push(Turn {
        id: uuid::Uuid::new_v4(),
        run_id: child_run_id,
        role: Role::Assistant,
        content: "Partial child output that should not be summarized".to_string(),
        reasoning: String::new(),
        sequence_number: 1,
        timestamp: Utc::now(),
    });
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
        sequence_number: 1,
        requested_at: Utc::now(),
        approved_at: Some(Utc::now()),
        completed_at: None,
    });

    StartupRecoveryFixture {
        session,
        parent_run_id,
        child_run_id,
    }
}
