use std::{io::Stdout, path::PathBuf, process::Stdio};

use agent_client_protocol::{self as acp, Agent};

use async_trait::async_trait;
use std::time::Duration;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use thiserror::Error;
use tokio::{process::Command, time::timeout};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;
pub type Result<T> = std::result::Result<T, FluentCodeError>;

const ACP_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum FluentCodeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("plugin error: {0}")]
    Plugin(String),

    #[error("invalid session data: {0}")]
    Session(String),
}

pub async fn run() -> Result<()> {
    let terminal = init()?;
    let mut client = FluentCodeClient {
        terminal,
        cwd: std::env::current_dir()?,
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let _ = client.run().await;
        })
        .await;
    restore(&mut client.terminal)
}

struct FluentCodeClient {
    terminal: AppTerminal,
    cwd: PathBuf,
}

impl FluentCodeClient {
    pub async fn run(&mut self) -> Result<()> {
        self.bootstrap().await?;
        println!("succeed to build session");
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok(())
    }

    async fn bootstrap(&mut self) -> Result<()> {
        let agent_path = default_agent_binary_path()?;
        let mut command = Command::new(&agent_path);
        command
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            FluentCodeError::Config(format!(
                "failed to launch ACP agent subprocess `{}`: {error}",
                agent_path.display()
            ))
        })?;
        let pid = child.id().ok_or_else(|| {
            FluentCodeError::Config(format!(
                "ACP subprocess `{}` did not expose a process id",
                agent_path.display()
            ))
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            FluentCodeError::Config(format!(
                "ACP subprocess `{}` did not provide a piped stdin handle",
                agent_path.display()
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            FluentCodeError::Config(format!(
                "ACP subprocess `{}` did not provide a piped stdout handle",
                agent_path.display()
            ))
        })?;
        let inner = ClientInner{};
        let (connection, io_future) = acp::ClientSideConnection::new(
            inner,
            stdin.compat_write(),
            stdout.compat(),
            |future| {
                tokio::task::spawn_local(future);
            },
        );
        let io_task = tokio::task::spawn_local(io_future);
        let capabilities = acp::ClientCapabilities::new().terminal(true).fs(acp::FileSystemCapabilities::new().read_text_file(true).write_text_file(true));
        let initialize_request = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
            .client_info(
                acp::Implementation::new("fluent-code-tui", env!("CARGO_PKG_VERSION"))
                    .title("fluent-code TUI"),
            )
            .client_capabilities(capabilities);

        let initialize_response = match timeout(
            ACP_INITIALIZE_TIMEOUT,
            connection.initialize(initialize_request),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                io_task.abort();
                let _ = io_task.await;
                return Err(FluentCodeError::Provider(format!(
                    "failed to initialize ACP connection over `{}`: {error}",
                    agent_path.display()
                )));
            }
            Err(_) => {
                io_task.abort();
                let _ = io_task.await;
                return Err(FluentCodeError::Provider(format!(
                    "timed out waiting for ACP initialize response from `{}`",
                    agent_path.display()
                )));
            }
        };

        Ok(())
    }
}

pub fn init() -> Result<AppTerminal> {
    enable_raw_mode()?;

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste,)?;

    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore(terminal: &mut AppTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn default_agent_binary_path() -> Result<PathBuf> {
    let current_exe = std::env::current_exe()?;
    let binary_name = format!("fluent-code-acp{}", std::env::consts::EXE_SUFFIX);
    let agent_path = current_exe.with_file_name(binary_name);
    if agent_path.exists() {
        Ok(agent_path)
    } else {
        Err(FluentCodeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "agent binary not found",
        )))
    }
}


struct ClientInner {
}

#[async_trait(?Send)]
impl acp::Client for ClientInner {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        todo!()
    }
    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }
}
