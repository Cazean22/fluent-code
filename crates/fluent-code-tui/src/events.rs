use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fluent_code_app::app::{AppStatus, Msg};

pub fn next_message(current_input: &str, status: &AppStatus) -> std::io::Result<Option<Msg>> {
    if !event::poll(Duration::from_millis(100))? {
        return Ok(None);
    }

    let event = event::read()?;
    let msg = map_event_to_message(event, current_input, status);

    Ok(msg)
}

pub fn map_event_to_message(event: Event, current_input: &str, status: &AppStatus) -> Option<Msg> {
    match event {
        Event::Key(KeyEvent { kind, .. }) if kind != KeyEventKind::Press => None,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL) => match status {
            AppStatus::Generating | AppStatus::AwaitingToolApproval | AppStatus::RunningTool => {
                Some(Msg::CancelActiveRun)
            }
            _ => Some(Msg::Quit),
        },
        Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL)
            && matches!(status, AppStatus::Idle | AppStatus::Error(_)) =>
        {
            Some(Msg::NewSession)
        }
        Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        }) => match status {
            AppStatus::Generating | AppStatus::AwaitingToolApproval | AppStatus::RunningTool => {
                Some(Msg::CancelActiveRun)
            }
            _ => Some(Msg::Quit),
        },
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            ..
        }) => match status {
            AppStatus::AwaitingToolApproval => Some(Msg::ApprovePendingTool),
            AppStatus::RunningTool => None,
            _ => Some(Msg::SubmitPrompt),
        },
        Event::Key(KeyEvent {
            code: KeyCode::Char('y'),
            modifiers,
            ..
        }) if modifiers.is_empty() && matches!(status, AppStatus::AwaitingToolApproval) => {
            Some(Msg::ApprovePendingTool)
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers,
            ..
        }) if modifiers.is_empty() && matches!(status, AppStatus::AwaitingToolApproval) => {
            Some(Msg::DenyPendingTool)
        }
        Event::Key(KeyEvent {
            code: KeyCode::Backspace,
            ..
        }) if matches!(status, AppStatus::Idle | AppStatus::Error(_)) => {
            let mut next = current_input.to_owned();
            next.pop();
            Some(Msg::InputChanged(next))
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            ..
        }) if (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
            && matches!(status, AppStatus::Idle | AppStatus::Error(_)) =>
        {
            let mut next = current_input.to_owned();
            next.push(ch);
            Some(Msg::InputChanged(next))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use fluent_code_app::app::{AppStatus, Msg};

    use super::map_event_to_message;

    #[test]
    fn ctrl_n_starts_new_session_only_when_idle_or_error() {
        let ctrl_n = Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });

        assert!(matches!(
            map_event_to_message(ctrl_n, "draft", &AppStatus::Idle),
            Some(Msg::NewSession)
        ));

        let ctrl_n = Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(
            map_event_to_message(ctrl_n, "draft", &AppStatus::Error("boom".to_string())),
            Some(Msg::NewSession)
        ));

        let ctrl_n = Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(map_event_to_message(ctrl_n, "draft", &AppStatus::Generating).is_none());
    }
}
