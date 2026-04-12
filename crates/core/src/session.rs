use std::collections::HashMap;

use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId {
    uuid: Uuid,
}

impl SessionId {
    pub fn new() -> Self {
        Self { uuid: Uuid::now_v7() }
    }
}

#[derive(Debug)]
pub enum Event {
    Error(String),
    Msg(String),
}

pub struct Session {
    pub id: SessionId,
    tx_event: UnboundedSender<Event>,
}

impl Session {
    pub fn new(tx_event: UnboundedSender<Event>) -> Self {
        Self { id: SessionId::new(), tx_event }
    }

    pub async fn run_turn(&mut self, _prompt: String) {
    }
}

pub struct SessionManager {
    pub sessions: HashMap<SessionId, Session>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self { sessions: HashMap::new() }
    }
}
