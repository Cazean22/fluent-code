use std::fs;
use std::io::Cursor;
use std::path::Path;

use serde_json::Value;

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

        Ok(ScriptedJsonlCapture {
            frames_processed,
            stdout,
            stdout_text,
            stdout_frames,
        })
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

    pub fn notification_frames(&self, method: &str) -> Vec<&Value> {
        self.stdout_frames
            .iter()
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }

    pub fn write_stdout(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        fs::write(path, self.stdout_bytes())
    }
}
