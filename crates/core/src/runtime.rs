use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{Event, Session, SessionId, SessionManager};

pub struct Runtime {
    session_id: Option<SessionId>,
    session_manager: SessionManager,
    rx_input: UnboundedReceiver<String>,
    tx_event: UnboundedSender<Event>,
}

impl Runtime {
    pub fn new(rx_input: UnboundedReceiver<String>, tx_event: UnboundedSender<Event>) -> Self {
        Self { session_id: None, rx_input, tx_event, session_manager: SessionManager::new() }
    }

    async fn start_turn(&mut self, prompt: String) {
        let session_id = if let Some(session_id) = self.session_id {
            session_id
        } else {
            let session = Session::new(self.tx_event.clone());
            let session_id = session.id;
            self.session_id = Some(session_id);
            self.session_manager.sessions.insert(session_id, session);
            session_id
        };
        let _ = self.tx_event.send(Event::Msg(prompt.clone()));

        self.session_manager
            .sessions
            .get_mut(&session_id)
            .expect("active session must exist in the session manager")
            .run_turn(prompt)
            .await;
    }

    pub async fn run(&mut self) {
        while let Some(prompt) = self.rx_input.recv().await {
            self.start_turn(prompt).await;
        }
    }
}
