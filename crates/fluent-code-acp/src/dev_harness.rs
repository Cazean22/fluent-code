use std::fs;
use std::io::{self, Cursor, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::server::ReaderEvent;
use crate::{AcpServer, FluentCodeError, Result};

#[derive(Debug, Clone, Default)]
pub struct ScriptedJsonlHarness;

impl ScriptedJsonlHarness {
    pub const fn new() -> Self {
        Self
    }

    pub async fn run_script(
        &self,
        server: &AcpServer,
        script: &str,
    ) -> Result<ScriptedJsonlCapture> {
        let mut reader = Cursor::new(script.as_bytes());
        let mut stdout = Vec::new();
        let frames_processed = server.serve_jsonl_script(&mut reader, &mut stdout).await?;
        ScriptedJsonlCapture::from_stdout(frames_processed, stdout)
    }

    pub fn start_live_session(&self, server: &AcpServer) -> LiveJsonlSession {
        LiveJsonlSession::start(server.clone())
    }
}

#[derive(Debug, Clone)]
pub struct ScriptedJsonlCapture {
    pub frames_processed: usize,
    stdout: Vec<u8>,
    stdout_text: String,
    stdout_frames: Vec<Value>,
}

impl ScriptedJsonlCapture {
    fn from_stdout(frames_processed: usize, stdout: Vec<u8>) -> Result<Self> {
        let stdout_text = String::from_utf8(stdout.clone()).map_err(|error| {
            FluentCodeError::Provider(format!("invalid UTF-8 stdout capture: {error}"))
        })?;
        let stdout_frames = stdout_text
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|error| {
                    FluentCodeError::Provider(format!(
                        "invalid ACP JSONL frame in stdout capture: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<Value>>>()?;

        Ok(Self {
            frames_processed,
            stdout,
            stdout_text,
            stdout_frames,
        })
    }

    pub fn stdout_bytes(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stdout_text(&self) -> &str {
        &self.stdout_text
    }

    pub fn stdout_frames(&self) -> &[Value] {
        &self.stdout_frames
    }

    pub fn response_frame(&self, id: u64) -> Option<&Value> {
        self.stdout_frames
            .iter()
            .find(|frame| frame.get("id").and_then(Value::as_u64) == Some(id))
    }

    pub fn response_ids(&self) -> Vec<u64> {
        self.stdout_frames
            .iter()
            .filter_map(|frame| frame.get("id").and_then(Value::as_u64))
            .collect()
    }

    pub fn notification_frames(&self, method: &str) -> Vec<&Value> {
        self.stdout_frames
            .iter()
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }

    pub fn frame_index_for_response(&self, id: u64) -> Option<usize> {
        self.stdout_frames
            .iter()
            .position(|frame| frame.get("id").and_then(Value::as_u64) == Some(id))
    }

    pub fn frame_indices_for_method(&self, method: &str) -> Vec<usize> {
        self.stdout_frames
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| {
                (frame.get("method").and_then(Value::as_str) == Some(method)).then_some(index)
            })
            .collect()
    }

    pub fn session_update_frames(&self) -> Vec<&Value> {
        self.notification_frames("session/update")
    }

    pub fn frame_indices_for_session_update_kind(&self, kind: &str) -> Vec<usize> {
        self.stdout_frames
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| {
                (frame["params"]["update"]["sessionUpdate"].as_str() == Some(kind)).then_some(index)
            })
            .collect()
    }

    pub fn session_update_kinds(&self) -> Vec<&str> {
        self.session_update_frames()
            .into_iter()
            .filter_map(|frame| frame["params"]["update"]["sessionUpdate"].as_str())
            .collect()
    }

    pub fn collect_agent_message_chunks(&self) -> String {
        self.collect_agent_message_chunk_texts().join("")
    }

    pub fn collect_agent_message_chunk_texts(&self) -> Vec<String> {
        self.session_update_frames()
            .into_iter()
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .map(str::to_owned)
            .collect()
    }

    pub fn write_stdout(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        fs::write(path, self.stdout_bytes())
    }
}

pub struct LiveJsonlSession {
    frame_sender: Option<mpsc::UnboundedSender<ReaderEvent>>,
    shared_capture: Arc<LiveCaptureState>,
    serve_task: JoinHandle<Result<usize>>,
}

impl LiveJsonlSession {
    fn start(server: AcpServer) -> Self {
        let (frame_sender, mut frame_receiver) = mpsc::unbounded_channel();
        let shared_capture = Arc::new(LiveCaptureState::default());
        let mut writer = LiveCaptureWriter::new(Arc::clone(&shared_capture));
        let serve_task = tokio::spawn(async move {
            server
                .serve_live_frames(&mut frame_receiver, &mut writer)
                .await
        });

        Self {
            frame_sender: Some(frame_sender),
            shared_capture,
            serve_task,
        }
    }

    pub fn send_frame(&self, frame: impl Into<String>) -> Result<()> {
        let frame_sender = self.frame_sender.as_ref().ok_or_else(|| {
            FluentCodeError::Provider("live ACP harness input is already closed".to_string())
        })?;
        frame_sender
            .send(ReaderEvent::Frame(frame.into()))
            .map_err(|_| FluentCodeError::Provider("failed to send live ACP frame".to_string()))
    }

    pub async fn wait_until<F>(
        &self,
        description: &str,
        predicate: F,
    ) -> Result<ScriptedJsonlCapture>
    where
        F: Fn(&ScriptedJsonlCapture) -> bool,
    {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.shared_capture.notify.notified();
                let capture = self.shared_capture.snapshot(0)?;
                if predicate(&capture) {
                    return Ok(capture);
                }

                notified.await;
            }
        })
        .await
        .map_err(|_| FluentCodeError::Provider(format!("timed out waiting for {description}")))?
    }

    pub async fn finish(mut self) -> Result<ScriptedJsonlCapture> {
        if let Some(frame_sender) = self.frame_sender.take() {
            frame_sender.send(ReaderEvent::Eof).map_err(|_| {
                FluentCodeError::Provider("failed to close live ACP input".to_string())
            })?;
        }

        let frames_processed = self.serve_task.await.map_err(|error| {
            FluentCodeError::Provider(format!("live ACP harness task failed: {error}"))
        })??;
        self.shared_capture.snapshot(frames_processed)
    }
}

#[derive(Default)]
struct LiveCaptureState {
    state: Mutex<LiveCaptureBuffer>,
    notify: Notify,
}

impl LiveCaptureState {
    fn snapshot(&self, frames_processed: usize) -> Result<ScriptedJsonlCapture> {
        let stdout = self
            .state
            .lock()
            .map_err(|_| FluentCodeError::Provider("live ACP capture mutex poisoned".to_string()))?
            .stdout
            .clone();
        ScriptedJsonlCapture::from_stdout(frames_processed, stdout)
    }
}

#[derive(Default)]
struct LiveCaptureBuffer {
    stdout: Vec<u8>,
    pending_line: Vec<u8>,
}

impl LiveCaptureBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<bool> {
        self.stdout.extend_from_slice(bytes);
        self.pending_line.extend_from_slice(bytes);

        let mut parsed_frame = false;
        while let Some(newline_index) = self.pending_line.iter().position(|byte| *byte == b'\n') {
            let mut line = self
                .pending_line
                .drain(..=newline_index)
                .collect::<Vec<_>>();
            line.pop();

            if matches!(line.last(), Some(b'\r')) {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }

            let line = String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            serde_json::from_str::<Value>(&line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            parsed_frame = true;
        }

        Ok(parsed_frame)
    }
}

struct LiveCaptureWriter {
    shared_capture: Arc<LiveCaptureState>,
}

impl LiveCaptureWriter {
    fn new(shared_capture: Arc<LiveCaptureState>) -> Self {
        Self { shared_capture }
    }
}

impl Write for LiveCaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let parsed_frame = {
            let mut state = self
                .shared_capture
                .state
                .lock()
                .map_err(|_| io::Error::other("live ACP capture mutex poisoned"))?;
            state.write(buf)?
        };

        if parsed_frame {
            self.shared_capture.notify.notify_waiters();
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
