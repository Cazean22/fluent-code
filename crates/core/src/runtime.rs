use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{Event, OpenAI, OpenAIConfig, Session, SessionId, SessionManager, StreamContent};

pub struct Runtime {
    session_id: Option<SessionId>,
    session_manager: SessionManager,
    rx_input: UnboundedReceiver<String>,
    tx_event: UnboundedSender<Event>,
    openai: OpenAI,
    tx_stream: UnboundedSender<StreamContent>,
    rx_stream: UnboundedReceiver<StreamContent>,
}

impl Runtime {
    pub fn new(rx_input: UnboundedReceiver<String>, tx_event: UnboundedSender<Event>) -> Self {
        let (tx_stream, rx_stream) = tokio::sync::mpsc::unbounded_channel();
        Self {
            session_id: None,
            rx_input,
            tx_event,
            session_manager: SessionManager::new(),
            tx_stream,
            rx_stream,
            openai: OpenAI::new(OpenAIConfig::default()),
        }
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
        {
            let openai = self.openai.clone();
            let tx_stream = self.tx_stream.clone();
            let prompt = prompt.clone();
            tokio::spawn(async move {
                openai.run(prompt, tx_stream).await;
            })
        };

        while let Some(msg) = self.rx_stream.recv().await {
            let msg_str = format!("{:?}", msg);
            let _ = self.tx_event.send(Event::Msg(msg_str));
        }

        self.session_manager
            .sessions
            .get_mut(&session_id)
            .expect("active session must exist in the session manager")
            .run_turn(prompt)
            .await;
    }

    pub async fn run(&mut self) {
        println!("start runtime");
        while let Some(prompt) = self.rx_input.recv().await {
            self.start_turn(prompt).await;
        }
    }
}
