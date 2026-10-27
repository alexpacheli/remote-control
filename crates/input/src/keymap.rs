//! Physical key → PC set-1 scancode (`0xE0xx` for extended keys).
//!
//! Scancodes name key *positions*, so the host's keyboard layout decides the
//! character: an ABNT2 viewer and a US host still agree on which key moved.

use winit::keyboard::KeyCode;

/// What the macOS Command key sends to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKey {
    /// Command acts as Ctrl, so Cmd+C copies on a Windows host.
    Ctrl,
    /// Command acts as the Windows key.
    Win,
}

/// Scancode for `code`, or `None` for keys with no PC equivalent.
pub fn scancode(code: KeyCode, command: CommandKey) -> Option<u16> {
    use KeyCode::*;
    let sc = match code {
        Escape => 0x01,
        Digit1 => 0x02,
        Digit2 => 0x03,
        Digit3 => 0x04,
        Digit4 => 0x05,
        Digit5 => 0x06,
        Digit6 => 0x07,
        Digit7 => 0x08,
        Digit8 => 0x09,
        Digit9 => 0x0A,
        Digit0 => 0x0B,
        Minus => 0x0C,
        Equal => 0x0D,
        Backspace => 0x0E,
        Tab => 0x0F,
        KeyQ => 0x10,
        KeyW => 0x11,
        KeyE => 0x12,
        KeyR => 0x13,
        KeyT => 0x14,
        KeyY => 0x15,
        KeyU => 0x16,
        KeyI => 0x17,
        KeyO => 0x18,
        KeyP => 0x19,
        BracketLeft => 0x1A,
        BracketRight => 0x1B,
        Enter => 0x1C,
        ControlLeft => 0x1D,
        KeyA => 0x1E,
        KeyS => 0x1F,
        KeyD => 0x20,
        KeyF => 0x21,
        KeyG => 0x22,
        KeyH => 0x23,
        KeyJ => 0x24,
        KeyK => 0x25,
        KeyL => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backquote => 0x29,
        ShiftLeft => 0x2A,
        Backslash => 0x2B,
        KeyZ => 0x2C,
        KeyX => 0x2D,
        KeyC => 0x2E,
        KeyV => 0x2F,
        KeyB => 0x30,
        KeyN => 0x31,
        KeyM => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        ShiftRight => 0x36,
        NumpadMultiply => 0x37,
        AltLeft => 0x38,
        Space => 0x39,
        CapsLock => 0x3A,
        F1 => 0x3B,
        F2 => 0x3C,
        F3 => 0x3D,
        F4 => 0x3E,
        F5 => 0x3F,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        ScrollLock => 0x46,
        Numpad7 => 0x47,
        Numpad8 => 0x48,
        Numpad9 => 0x49,
        NumpadSubtract => 0x4A,
        Numpad4 => 0x4B,
        Numpad5 => 0x4C,
        Numpad6 => 0x4D,
        NumpadAdd => 0x4E,
        Numpad1 => 0x4F,
        Numpad2 => 0x50,
        Numpad3 => 0x51,
        Numpad0 => 0x52,
        NumpadDecimal => 0x53,
        IntlBackslash => 0x56,
        F11 => 0x57,
        F12 => 0x58,
        NumpadEqual => 0x59,
        F13 => 0x64,
        F14 => 0x65,
        F15 => 0x66,
        F16 => 0x67,
        F17 => 0x68,
        F18 => 0x69,
        F19 => 0x6A,
        F20 => 0x6B,
        F21 => 0x6C,
        F22 => 0x6D,
        F23 => 0x6E,
        KanaMode => 0x70,
        // ABNT2 "/?" key, next to right Shift.
        IntlRo => 0x73,
        F24 => 0x76,
        Convert => 0x79,
        NonConvert => 0x7B,
        IntlYen => 0x7D,
        // ABNT2 numpad "." (the decimal key is ",").
        NumpadComma => 0x7E,
        MediaTrackPrevious => 0xE010,
        MediaTrackNext => 0xE019,
        NumpadEnter => 0xE01C,
        ControlRight => 0xE01D,
        AudioVolumeMute => 0xE020,
        MediaPlayPause => 0xE022,
        MediaStop => 0xE024,
        AudioVolumeDown => 0xE02E,
        AudioVolumeUp => 0xE030,
        NumpadDivide => 0xE035,
        PrintScreen => 0xE037,
        AltRight => 0xE038,
        NumLock => 0xE045,
        Home => 0xE047,
        ArrowUp => 0xE048,
        PageUp => 0xE049,
        ArrowLeft => 0xE04B,
        ArrowRight => 0xE04D,
        End => 0xE04F,
        ArrowDown => 0xE050,
        PageDown => 0xE051,
        Insert => 0xE052,
        Delete => 0xE053,
        SuperLeft if command == CommandKey::Ctrl && cfg!(target_os = "macos") => 0x1D,
        SuperRight if command == CommandKey::Ctrl && cfg!(target_os = "macos") => 0xE01D,
        SuperLeft => 0xE05B,
        SuperRight => 0xE05C,
        ContextMenu => 0xE05D,
        _ => return None,
    };
    Some(sc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(scancode(KeyCode::KeyA, CommandKey::Win), Some(0x1E));
        assert_eq!(scancode(KeyCode::Enter, CommandKey::Win), Some(0x1C));
        assert_eq!(scancode(KeyCode::ArrowLeft, CommandKey::Win), Some(0xE04B));
        assert_eq!(scancode(KeyCode::IntlRo, CommandKey::Win), Some(0x73));
        assert_eq!(scancode(KeyCode::SuperLeft, CommandKey::Win), Some(0xE05B));
        assert_eq!(scancode(KeyCode::Fn, CommandKey::Win), None);
    }

    #[test]
    fn command_as_ctrl_only_on_macos() {
        let expected = if cfg!(target_os = "macos") {
            0x1D
        } else {
            0xE05B
        };
        assert_eq!(
            scancode(KeyCode::SuperLeft, CommandKey::Ctrl),
            Some(expected)
        );
    }

    #[test]
    fn no_duplicate_scancodes() {
        use std::collections::HashMap;
        let all = [
            KeyCode::Escape,
            KeyCode::Digit1,
            KeyCode::KeyQ,
            KeyCode::KeyA,
            KeyCode::KeyZ,
            KeyCode::Enter,
            KeyCode::NumpadEnter,
            KeyCode::ControlLeft,
            KeyCode::ControlRight,
            KeyCode::AltLeft,
            KeyCode::AltRight,
            KeyCode::Numpad7,
            KeyCode::Home,
            KeyCode::Numpad8,
            KeyCode::ArrowUp,
            KeyCode::Slash,
            KeyCode::NumpadDivide,
            KeyCode::IntlRo,
            KeyCode::NumpadComma,
            KeyCode::NumpadDecimal,
            KeyCode::Delete,
            KeyCode::Insert,
        ];
        let mut seen = HashMap::new();
        for key in all {
            let sc = scancode(key, CommandKey::Win).unwrap();
            if let Some(other) = seen.insert(sc, key) {
                panic!("{key:?} and {other:?} share scancode {sc:#06x}");
            }
        }
    }
}
