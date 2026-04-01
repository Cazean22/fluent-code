use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fluent_code_app::SharedAppHost;
use fluent_code_app::agent::AgentRegistry;
use fluent_code_app::app::Msg;
use fluent_code_app::plugin::{PluginLoadSnapshot, ToolRegistry};
use fluent_code_app::runtime::Runtime;
use fluent_code_app::session::model::Session;
use fluent_code_app::session::store::FsSessionStore;
use fluent_code_provider::{MockProvider, ProviderClient};

#[tokio::test]
async fn wait_for_runtime_activity_returns_queued_message_without_sleep() {
    let root = unique_test_dir("wait-queued-exact");
    let mut host = new_host(&root, "wait queued exact");

    host.runtime_sender()
        .send(Msg::InputChanged("queued runtime message".to_string()))
        .expect("queue runtime message");

    assert!(
        tokio::time::timeout(Duration::from_millis(50), host.wait_for_runtime_activity())
            .await
            .expect("queued wait should complete immediately")
    );
    assert_eq!(host.state().draft_input, "");

    host.drain_runtime_messages()
        .await
        .expect("drain queued runtime message");

    assert_eq!(host.state().draft_input, "queued runtime message");

    cleanup(root);
}

#[tokio::test]
async fn wait_for_runtime_activity_wakes_on_new_message_after_idle() {
    let root = unique_test_dir("wait-idle-exact");
    let mut host = new_host(&root, "wait idle exact");

    assert!(
        tokio::time::timeout(Duration::from_millis(20), host.wait_for_runtime_activity())
            .await
            .is_err()
    );

    let runtime_sender = host.runtime_sender();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        runtime_sender
            .send(Msg::InputChanged("wake after idle wait".to_string()))
            .expect("send wake message");
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(200), host.wait_for_runtime_activity())
            .await
            .expect("idle wait should wake after runtime message")
    );
    assert_eq!(host.state().draft_input, "");

    host.drain_runtime_messages()
        .await
        .expect("drain wake message");

    assert_eq!(host.state().draft_input, "wake after idle wait");

    cleanup(root);
}

#[tokio::test]
async fn wait_for_runtime_activity_exits_cleanly_when_sender_closes() {
    let root = unique_test_dir("wait-close-exact");
    let mut host = new_host(&root, "wait close exact");

    host.close_runtime_activity_channel();

    assert!(
        !tokio::time::timeout(Duration::from_millis(50), host.wait_for_runtime_activity())
            .await
            .expect("closed runtime channel should exit promptly")
    );

    cleanup(root);
}

fn new_host(root: &std::path::Path, session_title: &str) -> SharedAppHost {
    let store = FsSessionStore::new(root.to_path_buf());
    let agent_registry = Arc::new(AgentRegistry::built_in().clone());
    let tool_registry = Arc::new(ToolRegistry::with_agent_registry(&agent_registry));
    let runtime = Runtime::new_with_tool_registry(
        ProviderClient::Mock(MockProvider::new(None)),
        Arc::clone(&tool_registry),
    );

    SharedAppHost::new(
        Session::new(session_title),
        store,
        runtime,
        agent_registry,
        tool_registry,
        PluginLoadSnapshot::default(),
    )
}

fn unique_test_dir(label: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!("fluent-code-app-{label}-{timestamp}"))
}

fn cleanup(path: PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}
