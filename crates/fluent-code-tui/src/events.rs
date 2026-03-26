use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fluent_code_app::app::permissions::PermissionReply;
use fluent_code_app::app::{AppStatus, Msg};

pub enum TuiAction {
    Message(Msg),
    ToggleToolDetails,
    ToggleHelpOverlay,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
}

pub fn next_action(current_input: &str, status: &AppStatus) -> std::io::Result<Option<TuiAction>> {
    if !event::poll(Duration::from_millis(0))? {
        return Ok(None);
    }

    let event = event::read()?;

    Ok(next_action_from_event(event, current_input, status))
}

pub fn next_action_from_event(
    event: Event,
    current_input: &str,
    status: &AppStatus,
) -> Option<TuiAction> {
    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::F(2),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::ToggleToolDetails);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::ToggleHelpOverlay);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::ScrollUp);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::ScrollDown);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::PageUp);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::PageDown);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::JumpTop);
    }

    if matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        })
    ) {
        return Some(TuiAction::JumpBottom);
    }

    map_event_to_message(event, current_input, status).map(TuiAction::Message)
}

pub fn map_event_to_message(event: Event, current_input: &str, status: &AppStatus) -> Option<Msg> {
    match event {
        Event::Paste(text) if matches!(status, AppStatus::Idle | AppStatus::Error(_)) => {
            let mut next = current_input.to_owned();
            next.push_str(&text);
            Some(Msg::InputChanged(next))
        }
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
            AppStatus::AwaitingToolApproval => Some(Msg::ReplyToPendingTool(PermissionReply::Once)),
            AppStatus::RunningTool => None,
            _ => Some(Msg::SubmitPrompt),
        },
        Event::Key(KeyEvent {
            code: KeyCode::Char('y'),
            modifiers,
            ..
        }) if modifiers.is_empty() && matches!(status, AppStatus::AwaitingToolApproval) => {
            Some(Msg::ReplyToPendingTool(PermissionReply::Once))
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers,
            ..
        }) if modifiers.is_empty() && matches!(status, AppStatus::AwaitingToolApproval) => {
            Some(Msg::ReplyToPendingTool(PermissionReply::Always))
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers,
            ..
        }) if modifiers.is_empty() && matches!(status, AppStatus::AwaitingToolApproval) => {
            Some(Msg::ReplyToPendingTool(PermissionReply::Deny))
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
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    };
    use fluent_code_app::app::{AppStatus, Msg};

    use super::{TuiAction, map_event_to_message, next_action_from_event};

    #[test]
    fn ctrl_n_starts_new_session_only_when_idle_or_error() {
        let ctrl_n = Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        assert!(matches!(
            map_event_to_message(ctrl_n, "draft", &AppStatus::Idle),
            Some(Msg::NewSession)
        ));

        let ctrl_n = Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(matches!(
            map_event_to_message(ctrl_n, "draft", &AppStatus::Error("boom".to_string())),
            Some(Msg::NewSession)
        ));

        let ctrl_n = Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(map_event_to_message(ctrl_n, "draft", &AppStatus::Generating).is_none());
    }

    #[test]
    fn f_keys_do_not_map_to_messages() {
        let f1 = Event::Key(KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let f2 = Event::Key(KeyEvent {
            code: KeyCode::F(2),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let f3 = Event::Key(KeyEvent {
            code: KeyCode::F(3),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        assert!(map_event_to_message(f1, "draft", &AppStatus::Idle).is_none());
        assert!(map_event_to_message(f2, "draft", &AppStatus::Idle).is_none());
        assert!(map_event_to_message(f3, "draft", &AppStatus::Idle).is_none());
    }

    #[test]
    fn navigation_keys_map_to_transcript_actions() {
        let cases = [
            (KeyCode::Up, TuiAction::ScrollUp),
            (KeyCode::Down, TuiAction::ScrollDown),
            (KeyCode::PageUp, TuiAction::PageUp),
            (KeyCode::PageDown, TuiAction::PageDown),
            (KeyCode::Home, TuiAction::JumpTop),
            (KeyCode::End, TuiAction::JumpBottom),
        ];

        for (code, expected) in cases {
            let event = Event::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });

            let action = next_action_from_event(event, "draft", &AppStatus::Idle);
            assert!(matches!(action, Some(actual) if same_action_variant(&actual, &expected)));
        }
    }

    #[test]
    fn mouse_wheel_events_are_ignored() {
        for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
            let event = Event::Mouse(MouseEvent {
                kind,
                column: 12,
                row: 4,
                modifiers: KeyModifiers::NONE,
            });

            assert!(next_action_from_event(event, "draft", &AppStatus::Idle).is_none());
        }
    }

    #[test]
    fn non_wheel_mouse_events_do_not_map_to_actions() {
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });

        assert!(next_action_from_event(event, "draft", &AppStatus::Idle).is_none());
    }

    #[test]
    fn paste_event_updates_input_in_one_message() {
        let event = Event::Paste(" world\nnext line".to_string());

        assert!(matches!(
            map_event_to_message(event, "hello", &AppStatus::Idle),
            Some(Msg::InputChanged(input)) if input == "hello world\nnext line"
        ));
    }

    #[test]
    fn paste_event_preserves_crlf_line_breaks() {
        let event = Event::Paste(" first\r\nsecond\rthird".to_string());

        assert!(matches!(
            map_event_to_message(event, "hello", &AppStatus::Idle),
            Some(Msg::InputChanged(input)) if input == "hello first\r\nsecond\rthird"
        ));
    }

    #[test]
    fn paste_event_is_ignored_while_tool_is_running() {
        let event = Event::Paste("ignored".to_string());

        assert!(map_event_to_message(event, "hello", &AppStatus::RunningTool).is_none());
    }

    fn same_action_variant(actual: &TuiAction, expected: &TuiAction) -> bool {
        matches!(
            (actual, expected),
            (TuiAction::ScrollUp, TuiAction::ScrollUp)
                | (TuiAction::ScrollDown, TuiAction::ScrollDown)
                | (TuiAction::PageUp, TuiAction::PageUp)
                | (TuiAction::PageDown, TuiAction::PageDown)
                | (TuiAction::JumpTop, TuiAction::JumpTop)
                | (TuiAction::JumpBottom, TuiAction::JumpBottom)
        )
    }
}
