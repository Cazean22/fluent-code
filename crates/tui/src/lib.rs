use std::{io::Stdout, time::Duration};

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fc_core::Runtime;
use ratatui::{Terminal, prelude::CrosstermBackend};

use tokio::sync::mpsc::unbounded_channel;
use utils::error::Result;

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn run() -> Result<()> {
    let mut terminal = init()?;
    let (tx_event, mut rx_event) = unbounded_channel();
    let (tx_input, rx_input) = unbounded_channel();
    let mut runtime = Runtime::new(rx_input, tx_event);
    tokio::spawn(async move {
        runtime.run().await;
    });
    {
        let tx_input = tx_input.clone();
        tokio::spawn(async move {
            loop {
                let _ = tx_input.send("Hi".to_string());
            }
        });
    }
    let _= tokio::spawn(async move {
        let start = std::time::Instant::now();
        let end = start + Duration::from_secs(10);
        println!("herer");
        while let Some(event) = rx_event.recv().await {
            println!("{:?}", event);
            if std::time::Instant::now() >= end {
                break;
            }
        }
    }).await;
    restore(&mut terminal)
}

fn init() -> Result<AppTerminal> {
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
