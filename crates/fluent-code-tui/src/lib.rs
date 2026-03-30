mod acp;
pub mod conversation;
pub mod events;
pub mod markdown_render;
mod terminal;
pub mod theme;
pub mod ui_state;
pub mod view;

use fluent_code_app::error::Result;

#[doc(hidden)]
pub use acp::{
    AcpClientRuntime, AcpFilesystemService, AcpLaunchOptions, AcpTerminalService,
    PendingPermissionProjection, PermissionOptionProjection, SubprocessStatus,
    TerminalCommandProbeResponse, TranscriptSource, TuiProjectionState, bootstrap_client_for_tests,
    initialize_default_session_for_tests,
};

fn merge_run_and_restore_results(run_result: Result<()>, restore_result: Result<()>) -> Result<()> {
    match (run_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Err(error), Err(_)) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

#[doc(hidden)]
pub async fn run_with_terminal_hooks_for_tests<
    TerminalType,
    InitTerminal,
    RestoreTerminal,
    RunWithTerminal,
    RunFuture,
>(
    init_terminal: InitTerminal,
    restore_terminal: RestoreTerminal,
    run_with_terminal: RunWithTerminal,
) -> Result<()>
where
    InitTerminal: FnOnce() -> Result<TerminalType>,
    RestoreTerminal: FnOnce(TerminalType) -> Result<()>,
    RunWithTerminal: FnOnce(&mut TerminalType) -> RunFuture,
    RunFuture: std::future::Future<Output = Result<()>>,
{
    let mut terminal = init_terminal()?;
    let run_result = run_with_terminal(&mut terminal).await;
    let restore_result = restore_terminal(terminal);

    merge_run_and_restore_results(run_result, restore_result)
}

pub async fn run() -> Result<()> {
    acp::run().await
}
