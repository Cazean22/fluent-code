use std::io::Stdout;

use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fc_core::Runtime;
use futures::StreamExt;
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
            let send_result = tx_input.send("Hi".to_string());
            if let Err(err) = send_result {
                eprintln!("Error sending input: {}", err);
            }
        });
    }
    let mut events = EventStream::new();
    loop {
        tokio::select! {
            Some(event) = rx_event.recv() => {
                println!("{:?}", event);
            }
            maybe_terminal_event = events.next() => {
                match maybe_terminal_event {
                    Some(Ok(event)) if is_terminate_shortcut(&event) => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                }
            }
        }
    }

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

fn is_terminate_shortcut(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL)
    )
}

#[cfg(test)]
mod tests {
    use super::is_terminate_shortcut;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    #[test]
    fn ctrl_c_terminates() {
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        assert!(is_terminate_shortcut(&event));
    }

    #[test]
    fn plain_c_does_not_terminate() {
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        assert!(!is_terminate_shortcut(&event));
    }
}
