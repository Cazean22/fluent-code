use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{FluentCodeError, Result};
use crate::logging::path_for_log;
use crate::session::model::{
    ForegroundOwnerRecord, RunRecord, Session, SessionId, ToolInvocationRecord, Turn,
};

pub trait SessionStore {
    fn create(&self, session: &Session) -> Result<()>;
    fn load(&self, id: &SessionId) -> Result<Session>;
    fn save(&self, session: &Session) -> Result<()>;
    fn append_turn(&self, session_id: &SessionId, turn: &Turn) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct FsSessionStore {
    root: PathBuf,
}

impl FsSessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn load_or_create_latest(&self) -> Result<Session> {
        self.ensure_root()?;

        match self.read_latest_session_id()? {
            Some(id) => {
                let meta_path = self.session_meta_path(&id);
                info!(
                    session_id = %id,
                    store_root = %path_for_log(&self.root),
                    "loading latest persisted session"
                );
                if !meta_path.exists() {
                    warn!(
                        session_id = %id,
                        store_root = %path_for_log(&self.root),
                        session_meta_path = %path_for_log(&meta_path),
                        latest_session_path = %path_for_log(&self.latest_session_path()),
                        "latest session metadata missing; creating a new session"
                    );
                    self.create_new_session()
                } else {
                    self.load(&id)
                }
            }
            None => {
                info!(
                    store_root = %path_for_log(&self.root),
                    latest_session_path = %path_for_log(&self.latest_session_path()),
                    "no persisted latest session found; creating a new session"
                );
                self.create_new_session()
            }
        }
    }

    pub fn create_new_session(&self) -> Result<Session> {
        self.ensure_root()?;

        let session = Session::new("New Session");
        self.create(&session)?;
        info!(
            session_id = %session.id,
            session_title = %session.title,
            session_dir = %path_for_log(&self.session_dir(&session.id)),
            "created new session"
        );
        Ok(session)
    }

    fn ensure_root(&self) -> Result<()> {
        fs::create_dir_all(self.sessions_root())?;
        debug!(root = %path_for_log(&self.root), "ensured session store root exists");
        Ok(())
    }

    fn sessions_root(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn latest_session_path(&self) -> PathBuf {
        self.root.join("latest_session")
    }

    fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.sessions_root().join(id.to_string())
    }

    fn session_meta_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("session.json")
    }

    fn turns_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("turns.jsonl")
    }

    fn write_latest_session_id(&self, id: &SessionId) -> Result<()> {
        fs::write(self.latest_session_path(), id.to_string())?;
        debug!(
            session_id = %id,
            latest_session_path = %path_for_log(&self.latest_session_path()),
            "updated latest session pointer"
        );
        Ok(())
    }

    fn read_latest_session_id(&self) -> Result<Option<SessionId>> {
        let path = self.latest_session_path();
        if !path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(path)?;
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let id = trimmed
            .parse()
            .map_err(|err| FluentCodeError::Session(format!("invalid latest session id: {err}")))?;
        Ok(Some(id))
    }

    fn read_turns(&self, path: &Path) -> Result<Vec<Turn>> {
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut turns = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            turns.push(serde_json::from_str(&line)?);
        }

        Ok(turns)
    }
}

impl SessionStore for FsSessionStore {
    fn create(&self, session: &Session) -> Result<()> {
        self.ensure_root()?;
        fs::create_dir_all(self.session_dir(&session.id))?;
        self.save(session)?;
        self.write_latest_session_id(&session.id)
    }

    fn load(&self, id: &SessionId) -> Result<Session> {
        let meta_path = self.session_meta_path(id);
        if !meta_path.exists() {
            warn!(
                session_id = %id,
                session_meta_path = %path_for_log(&meta_path),
                "session metadata file missing during load"
            );
            return Err(FluentCodeError::Session(format!(
                "session metadata not found for {id}"
            )));
        }

        let metadata: SessionMetadata = serde_json::from_str(&fs::read_to_string(meta_path)?)?;
        let turns = self.read_turns(&self.turns_path(id))?;

        let mut session = Session {
            id: metadata.id,
            title: metadata.title,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            next_replay_sequence: metadata.next_replay_sequence,
            permissions: metadata.permissions,
            runs: metadata.runs,
            turns,
            tool_invocations: metadata.tool_invocations,
            foreground_owner: metadata.foreground_owner,
        };
        session.normalize_persistence();

        info!(
            session_id = %session.id,
            session_title = %session.title,
            turn_count = session.turns.len(),
            run_count = session.runs.len(),
            tool_invocation_count = session.tool_invocations.len(),
            "loaded session from disk"
        );

        Ok(session)
    }

    fn save(&self, session: &Session) -> Result<()> {
        self.ensure_root()?;
        fs::create_dir_all(self.session_dir(&session.id))?;

        let metadata = SessionMetadata::from(session);
        fs::write(
            self.session_meta_path(&session.id),
            serde_json::to_vec_pretty(&metadata)?,
        )?;

        let mut turns_file = File::create(self.turns_path(&session.id))?;
        for turn in &session.turns {
            writeln!(turns_file, "{}", serde_json::to_string(turn)?)?;
        }

        info!(
            session_id = %session.id,
            session_title = %session.title,
            turn_count = session.turns.len(),
            run_count = session.runs.len(),
            tool_invocation_count = session.tool_invocations.len(),
            "saved session snapshot"
        );

        self.write_latest_session_id(&session.id)
    }

    fn append_turn(&self, session_id: &SessionId, turn: &Turn) -> Result<()> {
        self.ensure_root()?;
        fs::create_dir_all(self.session_dir(session_id))?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.turns_path(session_id))?;
        writeln!(file, "{}", serde_json::to_string(turn)?)?;
        debug!(
            session_id = %session_id,
            turn_id = %turn.id,
            run_id = %turn.run_id,
            role = ?turn.role,
            "appended session turn"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadata {
    id: SessionId,
    title: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    next_replay_sequence: crate::session::model::ReplaySequence,
    #[serde(default)]
    permissions: crate::session::model::SessionPermissionState,
    #[serde(default)]
    runs: Vec<RunRecord>,
    #[serde(default)]
    tool_invocations: Vec<ToolInvocationRecord>,
    #[serde(default)]
    foreground_owner: Option<ForegroundOwnerRecord>,
}

impl From<&Session> for SessionMetadata {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id,
            title: session.title.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            next_replay_sequence: session.next_replay_sequence,
            permissions: session.permissions.clone(),
            runs: session.runs.clone(),
            tool_invocations: session.tool_invocations.clone(),
            foreground_owner: session.foreground_owner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Utc;
    use uuid::Uuid;

    use super::{FsSessionStore, SessionStore};
    use crate::error::FluentCodeError;
    use crate::session::model::{
        ForegroundOwnerRecord, ForegroundPhase, Role, RunStatus, RunTerminalStopReason, Session,
        TaskDelegationRecord, TaskDelegationStatus, ToolApprovalState, ToolExecutionState,
        ToolInvocationRecord, ToolSource, Turn,
    };

    #[test]
    fn creates_and_loads_latest_session() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let created = store
            .load_or_create_latest()
            .expect("create latest session");
        let loaded = store.load_or_create_latest().expect("load latest session");

        assert_eq!(created.id, loaded.id);
        cleanup(root);
    }

    #[test]
    fn create_new_session_persists_and_updates_latest_pointer() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let first = store.create_new_session().expect("create first session");
        let second = store.create_new_session().expect("create second session");

        assert_ne!(first.id, second.id);

        let latest = store.load_or_create_latest().expect("load latest session");
        assert_eq!(latest.id, second.id);
        assert_eq!(latest.title, "New Session");
        assert!(latest.turns.is_empty());
        assert!(latest.runs.is_empty());
        assert!(latest.tool_invocations.is_empty());

        cleanup(root);
    }

    #[test]
    fn saves_and_restores_turns() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let mut session = Session::new("test session");
        let turn_sequence = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            role: Role::User,
            content: "hello".to_string(),
            reasoning: String::new(),
            sequence_number: turn_sequence,
            timestamp: Utc::now(),
        });

        store.create(&session).expect("create session");
        let loaded = store.load(&session.id).expect("load session");

        assert_eq!(loaded.turns.len(), 1);
        assert_eq!(loaded.turns[0].content, "hello");
        cleanup(root);
    }

    #[test]
    fn saves_and_restores_foreground_owner() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let mut session = Session::new("foreground owner session");
        let run_id = Uuid::new_v4();
        let batch_anchor_turn_id = Uuid::new_v4();
        session.foreground_owner = Some(ForegroundOwnerRecord {
            run_id,
            phase: ForegroundPhase::AwaitingToolApproval,
            batch_anchor_turn_id: Some(batch_anchor_turn_id),
        });

        store
            .create(&session)
            .expect("create session with foreground owner");
        let loaded = store
            .load(&session.id)
            .expect("load session with foreground owner");

        assert_eq!(
            loaded.foreground_owner,
            Some(ForegroundOwnerRecord {
                run_id,
                phase: ForegroundPhase::AwaitingToolApproval,
                batch_anchor_turn_id: Some(batch_anchor_turn_id),
            })
        );

        cleanup(root);
    }

    #[test]
    fn loads_legacy_session_metadata_without_runs() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let session = Session::new("legacy session");
        let session_dir = root.join("sessions").join(session.id.to_string());
        std::fs::create_dir_all(&session_dir).expect("create legacy session dir");

        std::fs::write(root.join("latest_session"), session.id.to_string())
            .expect("write latest session id");

        std::fs::write(
            session_dir.join("session.json"),
            format!(
                concat!(
                    "{{\n",
                    "  \"id\": \"{}\",\n",
                    "  \"title\": \"{}\",\n",
                    "  \"created_at\": \"{}\",\n",
                    "  \"updated_at\": \"{}\"\n",
                    "}}\n"
                ),
                session.id,
                session.title,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
            ),
        )
        .expect("write legacy session metadata");

        std::fs::write(session_dir.join("turns.jsonl"), "").expect("write turns file");

        let loaded = store.load(&session.id).expect("load legacy session");
        assert!(loaded.runs.is_empty());
        assert!(loaded.turns.is_empty());
        assert!(loaded.tool_invocations.is_empty());

        cleanup(root);
    }

    #[test]
    fn loads_legacy_turns_without_reasoning_field() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let session = Session::new("legacy turns");
        let session_dir = root.join("sessions").join(session.id.to_string());
        std::fs::create_dir_all(&session_dir).expect("create legacy session dir");

        std::fs::write(root.join("latest_session"), session.id.to_string())
            .expect("write latest session id");

        std::fs::write(
            session_dir.join("session.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": session.id,
                "title": session.title,
                "created_at": session.created_at,
                "updated_at": session.updated_at,
                "runs": [],
                "tool_invocations": []
            }))
            .expect("serialize legacy session metadata"),
        )
        .expect("write session metadata");

        std::fs::write(
            session_dir.join("turns.jsonl"),
            format!(
                concat!(
                    "{{",
                    "\"id\":\"{}\",",
                    "\"run_id\":\"{}\",",
                    "\"role\":\"Assistant\",",
                    "\"content\":\"hello\",",
                    "\"timestamp\":\"{}\"",
                    "}}\n"
                ),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Utc::now().to_rfc3339(),
            ),
        )
        .expect("write legacy turn row");

        let loaded = store.load(&session.id).expect("load legacy turns");

        assert_eq!(loaded.turns.len(), 1);
        assert_eq!(loaded.turns[0].content, "hello");
        assert_eq!(loaded.turns[0].reasoning, "");

        cleanup(root);
    }

    #[test]
    fn saves_and_restores_tool_invocations() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let mut session = Session::new("tool session");
        let invocation_sequence = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            tool_call_id: "call-1".to_string(),
            tool_name: "uppercase_text".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "text": "hello" }),
            preceding_turn_id: None,
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("HELLO".to_string()),
            error: None,
            delegation: None,
            sequence_number: invocation_sequence,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        });

        store
            .create(&session)
            .expect("create session with tool invocation");
        let loaded = store
            .load(&session.id)
            .expect("load session with tool invocation");

        assert_eq!(loaded.tool_invocations.len(), 1);
        assert_eq!(loaded.tool_invocations[0].tool_name, "uppercase_text");
        assert_eq!(loaded.tool_invocations[0].result.as_deref(), Some("HELLO"));
        assert!(loaded.tool_invocations[0].task_delegation().is_none());

        cleanup(root);
    }

    #[test]
    fn loads_legacy_tool_invocation_delegation_fields() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let session = Session::new("legacy tool delegation");
        let session_dir = root.join("sessions").join(session.id.to_string());
        let child_run_id = Uuid::new_v4();

        std::fs::create_dir_all(&session_dir).expect("create legacy session dir");
        std::fs::write(
            session_dir.join("session.json"),
            serde_json::json!({
                "id": session.id,
                "title": session.title,
                "created_at": session.created_at,
                "updated_at": session.updated_at,
                "permissions": { "rules": [] },
                "runs": [],
                "tool_invocations": [
                    {
                        "id": Uuid::new_v4(),
                        "run_id": Uuid::new_v4(),
                        "tool_call_id": "task-call-1",
                        "tool_name": "task",
                        "tool_source": "built_in",
                        "arguments": { "agent": "explore", "prompt": "Inspect state" },
                        "approval_state": "approved",
                        "execution_state": "running",
                        "result": null,
                        "error": null,
                        "child_run_id": child_run_id,
                        "delegation_agent_name": "explore",
                        "delegation_prompt": "Inspect state",
                        "requested_at": Utc::now(),
                        "approved_at": null,
                        "completed_at": null
                    }
                ]
            })
            .to_string(),
        )
        .expect("write legacy session metadata with delegation");

        let loaded = store
            .load(&session.id)
            .expect("load legacy delegation metadata");

        assert_eq!(loaded.tool_invocations.len(), 1);
        let delegation = loaded.tool_invocations[0]
            .task_delegation()
            .expect("delegation restored from legacy fields");
        assert_eq!(delegation.child_run_id, Some(child_run_id));
        assert_eq!(delegation.agent_name.as_deref(), Some("explore"));
        assert_eq!(delegation.prompt.as_deref(), Some("Inspect state"));
        assert_eq!(delegation.status, TaskDelegationStatus::Running);

        cleanup(root);
    }

    #[test]
    fn saves_structured_tool_invocation_delegation() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let mut session = Session::new("delegated tool session");
        let invocation_sequence = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            tool_call_id: "task-call-1".to_string(),
            tool_name: "task".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "agent": "explore", "prompt": "Inspect state" }),
            preceding_turn_id: None,
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Running,
            result: None,
            error: None,
            delegation: Some(TaskDelegationRecord {
                child_run_id: Some(Uuid::new_v4()),
                agent_name: Some("explore".to_string()),
                prompt: Some("Inspect state".to_string()),
                status: TaskDelegationStatus::Running,
            }),
            sequence_number: invocation_sequence,
            requested_at: Utc::now(),
            approved_at: Some(Utc::now()),
            completed_at: None,
        });

        store
            .create(&session)
            .expect("create session with structured delegation");
        let saved = std::fs::read_to_string(
            root.join("sessions")
                .join(session.id.to_string())
                .join("session.json"),
        )
        .expect("read saved session metadata");

        assert!(saved.contains("\"delegation\""));
        assert!(!saved.contains("\"delegation_agent_name\""));
        assert!(!saved.contains("\"delegation_prompt\""));
        assert!(saved.contains("\"child_run_id\":"));
        assert!(saved.contains("\"status\": \"running\""));

        cleanup(root);
    }

    #[test]
    fn save_and_load_preserves_replay_sequence_order_and_stop_reason() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());

        let mut session = Session::new("ordered replay session");
        let run_id = Uuid::new_v4();
        let shared_timestamp = Utc::now();
        session.upsert_run(run_id, RunStatus::InProgress);

        let assistant_turn_id = Uuid::new_v4();
        let first_turn_sequence = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: assistant_turn_id,
            run_id,
            role: Role::Assistant,
            content: "second in time, first in replay".to_string(),
            reasoning: String::new(),
            sequence_number: first_turn_sequence,
            timestamp: shared_timestamp + chrono::Duration::seconds(2),
        });
        let tool_sequence = session.allocate_replay_sequence();
        session.tool_invocations.push(ToolInvocationRecord {
            id: Uuid::new_v4(),
            run_id,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            tool_source: ToolSource::BuiltIn,
            arguments: serde_json::json!({ "path": "src/main.rs" }),
            preceding_turn_id: Some(assistant_turn_id),
            approval_state: ToolApprovalState::Approved,
            execution_state: ToolExecutionState::Completed,
            result: Some("ok".to_string()),
            error: None,
            delegation: None,
            sequence_number: tool_sequence,
            requested_at: shared_timestamp - chrono::Duration::seconds(5),
            approved_at: Some(shared_timestamp - chrono::Duration::seconds(4)),
            completed_at: Some(shared_timestamp - chrono::Duration::seconds(3)),
        });
        let second_turn_sequence = session.allocate_replay_sequence();
        session.turns.push(Turn {
            id: Uuid::new_v4(),
            run_id,
            role: Role::Assistant,
            content: "earlier in time, later in replay".to_string(),
            reasoning: String::new(),
            sequence_number: second_turn_sequence,
            timestamp: shared_timestamp - chrono::Duration::seconds(10),
        });
        session.upsert_run_with_stop_reason(
            run_id,
            RunStatus::Failed,
            Some(RunTerminalStopReason::Interrupted),
        );

        store
            .create(&session)
            .expect("create ordered replay session");
        let loaded = store
            .load(&session.id)
            .expect("load ordered replay session");

        let mut replay_items = loaded
            .turns
            .iter()
            .map(|turn| (turn.sequence_number, format!("turn:{}", turn.content)))
            .chain(loaded.tool_invocations.iter().map(|invocation| {
                (
                    invocation.sequence_number,
                    format!("tool:{}", invocation.tool_name),
                )
            }))
            .collect::<Vec<_>>();
        replay_items.sort_by_key(|(sequence_number, _)| *sequence_number);

        assert_eq!(
            replay_items,
            vec![
                (
                    loaded.turns[0].sequence_number,
                    "turn:second in time, first in replay".to_string(),
                ),
                (
                    loaded.tool_invocations[0].sequence_number,
                    "tool:read".to_string(),
                ),
                (
                    loaded.turns[1].sequence_number,
                    "turn:earlier in time, later in replay".to_string(),
                ),
            ]
        );
        let run = loaded.find_run(run_id).expect("run restored");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(
            run.terminal_stop_reason,
            Some(RunTerminalStopReason::Interrupted)
        );
        assert!(run.terminal_sequence.is_some());

        cleanup(root);
    }

    #[test]
    fn stale_latest_session_pointer_creates_new_session() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let stale_id = Uuid::new_v4();

        std::fs::create_dir_all(root.join("sessions")).expect("create sessions root");
        std::fs::write(root.join("latest_session"), stale_id.to_string())
            .expect("write stale latest session id");

        let created = store
            .load_or_create_latest()
            .expect("create session for stale latest pointer");

        assert_ne!(created.id, stale_id);
        assert_eq!(created.title, "New Session");
        assert!(store.session_meta_path(&created.id).exists());
        assert_eq!(
            std::fs::read_to_string(root.join("latest_session")).expect("read latest session id"),
            created.id.to_string()
        );

        let reloaded = store
            .load_or_create_latest()
            .expect("load replacement latest session");
        assert_eq!(reloaded.id, created.id);

        cleanup(root);
    }

    #[test]
    fn load_errors_when_session_metadata_is_missing() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let session_id = Uuid::new_v4();

        let err = store
            .load(&session_id)
            .expect_err("missing session metadata should error");

        assert_eq!(
            err.to_string(),
            format!("invalid session data: session metadata not found for {session_id}")
        );

        cleanup(root);
    }

    #[test]
    fn malformed_latest_session_metadata_still_errors() {
        let root = unique_test_dir();
        let store = FsSessionStore::new(root.clone());
        let session_id = Uuid::new_v4();
        let session_dir = root.join("sessions").join(session_id.to_string());

        std::fs::create_dir_all(&session_dir).expect("create session dir");
        std::fs::write(root.join("latest_session"), session_id.to_string())
            .expect("write latest session id");
        std::fs::write(session_dir.join("session.json"), "{not valid json")
            .expect("write malformed session metadata");

        let err = store
            .load_or_create_latest()
            .expect_err("malformed latest session metadata should error");

        assert!(matches!(err, FluentCodeError::SerdeJson(_)));

        cleanup(root);
    }

    fn unique_test_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();

        std::env::temp_dir().join(format!("fluent-code-test-{nanos}"))
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_dir_all(path);
    }
}
