//! Encoding crossterm key and mouse events back into the bytes a terminal
//! sends, for input forwarded to the shell without its original bytes.

use crossterm::event::{KeyEvent, KeyModifiers};
#[cfg(any(not(unix), test))]
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

#[cfg(any(not(unix), test))]
pub(super) fn encode_mouse_event_for_pty(mouse: MouseEvent) -> Vec<u8> {
    let (mut cb, final_byte) = sgr_mouse_code(mouse.kind);
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    format!(
        "\x1b[<{};{};{}{}",
        cb,
        mouse.column.saturating_add(1),
        mouse.row.saturating_add(1),
        final_byte
    )
    .into_bytes()
}

#[cfg(any(not(unix), test))]
fn sgr_mouse_code(kind: MouseEventKind) -> (u16, char) {
    let button_code = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    match kind {
        MouseEventKind::Down(button) => (button_code(button), 'M'),
        MouseEventKind::Up(button) => (button_code(button), 'm'),
        MouseEventKind::Drag(button) => (button_code(button) + 32, 'M'),
        MouseEventKind::Moved => (35, 'M'),
        MouseEventKind::ScrollUp => (64, 'M'),
        MouseEventKind::ScrollDown => (65, 'M'),
        MouseEventKind::ScrollLeft => (66, 'M'),
        MouseEventKind::ScrollRight => (67, 'M'),
    }
}

pub(super) fn encode_key_for_pty(key: &KeyEvent) -> Vec<u8> {
    use crossterm::event::KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let nav_modifier = navigation_modifier_param(key);
    let mut out = Vec::new();
    let push_alt = |out: &mut Vec<u8>| out.push(0x1B);

    match key.code {
        Char(c) => {
            if alt {
                push_alt(&mut out);
            }
            if ctrl {
                let lc = c.to_ascii_lowercase();
                let code = match lc {
                    'a'..='z' => Some((lc as u8) - b'a' + 1),
                    '@' | ' ' => Some(0),
                    '[' | '\\' | ']' | '^' | '_' => Some((lc as u8) - 0x40),
                    _ => None,
                };
                if let Some(b) = code {
                    out.push(b);
                } else {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        Enter => out.push(b'\r'),
        Tab => out.push(b'\t'),
        BackTab => out.extend_from_slice(b"\x1b[Z"),
        Backspace if alt => out.extend_from_slice(b"\x1b\x7f"),
        Backspace => out.push(0x7f),
        Esc => out.push(0x1b),
        Left => push_navigation_csi(&mut out, nav_modifier, b'D', b"\x1b[D"),
        Right => push_navigation_csi(&mut out, nav_modifier, b'C', b"\x1b[C"),
        Up => push_navigation_csi(&mut out, nav_modifier, b'A', b"\x1b[A"),
        Down => push_navigation_csi(&mut out, nav_modifier, b'B', b"\x1b[B"),
        Home => push_navigation_csi(&mut out, nav_modifier, b'H', b"\x1b[H"),
        End => push_navigation_csi(&mut out, nav_modifier, b'F', b"\x1b[F"),
        PageUp => push_navigation_tilde_csi(&mut out, nav_modifier, 5, b"\x1b[5~"),
        PageDown => push_navigation_tilde_csi(&mut out, nav_modifier, 6, b"\x1b[6~"),
        Insert => push_navigation_tilde_csi(&mut out, nav_modifier, 2, b"\x1b[2~"),
        Delete => push_navigation_tilde_csi(&mut out, nav_modifier, 3, b"\x1b[3~"),
        F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
        _ => {}
    }
    out
}

fn navigation_modifier_param(key: &KeyEvent) -> Option<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (shift, alt, ctrl) {
        (false, false, false) => None,
        (true, false, false) => Some(2),
        (false, true, false) => Some(3),
        (true, true, false) => Some(4),
        (false, false, true) => Some(5),
        (true, false, true) => Some(6),
        (false, true, true) => Some(7),
        (true, true, true) => Some(8),
    }
}

fn push_modified_csi(out: &mut Vec<u8>, final_byte: u8, modifier: u8) {
    out.extend_from_slice(format!("\x1b[1;{modifier}").as_bytes());
    out.push(final_byte);
}

fn push_modified_tilde_csi(out: &mut Vec<u8>, code: u8, modifier: u8) {
    out.extend_from_slice(format!("\x1b[{code};{modifier}~").as_bytes());
}

fn push_navigation_csi(out: &mut Vec<u8>, modifier: Option<u8>, final_byte: u8, plain: &[u8]) {
    if let Some(modifier) = modifier {
        push_modified_csi(out, final_byte, modifier);
    } else {
        out.extend_from_slice(plain);
    }
}

fn push_navigation_tilde_csi(out: &mut Vec<u8>, modifier: Option<u8>, code: u8, plain: &[u8]) {
    if let Some(modifier) = modifier {
        push_modified_tilde_csi(out, code, modifier);
    } else {
        out.extend_from_slice(plain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_mouse_event_for_pty_uses_sgr_mouse_coordinates() {
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 9,
            row: 4,
            modifiers: KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        };
        assert_eq!(encode_mouse_event_for_pty(mouse), b"\x1b[<20;10;5M");

        let mouse = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse_event_for_pty(mouse), b"\x1b[<2;1;1m");

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 2,
            row: 3,
            modifiers: KeyModifiers::ALT,
        };
        assert_eq!(encode_mouse_event_for_pty(mouse), b"\x1b[<73;3;4M");
    }

    #[test]
    fn encode_key_for_pty_preserves_alt_arrows() {
        use crossterm::event::KeyCode;

        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            b"\x1b[1;3D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            b"\x1b[1;3C"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            b"\x1b[1;3A"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            b"\x1b[1;3B"
        );
    }

    #[test]
    fn encode_key_for_pty_preserves_modified_navigation_keys() {
        use crossterm::event::KeyCode;

        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            b"\x1b[1;5C"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)),
            b"\x1b[1;5H"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
            b"\x1b[1;5F"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
            b"\x1b[3;5~"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)),
            b"\x1b[1;2D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::ALT | KeyModifiers::CONTROL
            )),
            b"\x1b[1;7D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
            b"\x1b\x7f"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL)),
            &[0x1c]
        );
    }
}
