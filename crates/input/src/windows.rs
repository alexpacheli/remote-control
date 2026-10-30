//! Input injection with `SendInput`.
//!
//! Note: Windows does not let a normal process inject into elevated windows
//! or the secure desktop; that needs the SYSTEM service planned for F4.

// Win32 calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::collections::HashSet;

use hext_proto::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WHEEL_DELTA, XBUTTON1, XBUTTON2,
};

use crate::{Area, Injector};

#[derive(Default)]
pub struct SendInputInjector {
    keys_down: HashSet<u16>,
    buttons_down: HashSet<ButtonId>,
    /// Sub-notch scroll left over from trackpads, in WHEEL_DELTA units.
    wheel_rest: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ButtonId {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl From<MouseButton> for ButtonId {
    fn from(b: MouseButton) -> Self {
        match b {
            MouseButton::Left => Self::Left,
            MouseButton::Right => Self::Right,
            MouseButton::Middle => Self::Middle,
            MouseButton::Back => Self::Back,
            MouseButton::Forward => Self::Forward,
        }
    }
}

impl Injector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent, area: Area) {
        match *event {
            InputEvent::MouseMove { x, y } => move_to(area.to_pixel(x, y)),
            InputEvent::MouseButton { button, pressed } => {
                let id = ButtonId::from(button);
                if pressed {
                    self.buttons_down.insert(id);
                } else {
                    self.buttons_down.remove(&id);
                }
                send_button(id, pressed);
            }
            InputEvent::Wheel { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return;
                }
                let delta = WHEEL_DELTA as f32;
                self.wheel_rest.0 += dx * delta;
                self.wheel_rest.1 += dy * delta;
                let (h, v) = (self.wheel_rest.0.trunc(), self.wheel_rest.1.trunc());
                self.wheel_rest.0 -= h;
                self.wheel_rest.1 -= v;
                if v != 0.0 {
                    send_mouse(MOUSEEVENTF_WHEEL, 0, 0, v as i32 as u32);
                }
                if h != 0.0 {
                    send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, h as i32 as u32);
                }
            }
            InputEvent::Key { scancode, pressed } => {
                if pressed {
                    self.keys_down.insert(scancode);
                } else {
                    self.keys_down.remove(&scancode);
                }
                send_key(scancode, pressed);
            }
            InputEvent::ReleaseAll => self.release_all(),
            // Sent by the service (`SendSAS`), not injected.
            InputEvent::SecureAttention => {}
        }
    }

    fn release_all(&mut self) {
        for sc in self.keys_down.drain() {
            send_key(sc, false);
        }
        for button in self.buttons_down.drain() {
            send_button(button, false);
        }
    }
}

fn move_to((px, py): (i32, i32)) {
    // Absolute coordinates are normalized to 0..=65535 over the virtual desktop.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let nx = ((px - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
    let ny = ((py - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
    send_mouse(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        nx,
        ny,
        0,
    );
}

fn send_button(button: ButtonId, pressed: bool) {
    let (flags, data) = match (button, pressed) {
        (ButtonId::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (ButtonId::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (ButtonId::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (ButtonId::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (ButtonId::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (ButtonId::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (ButtonId::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (ButtonId::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (ButtonId::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (ButtonId::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    };
    send_mouse(flags, 0, 0, data as u32);
}

fn send_mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) {
    send(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn send_key(scancode: u16, pressed: bool) {
    let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
    if scancode & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    send(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode & 0xFF,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn send(input: INPUT) {
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent != 1 {
        tracing::debug!("SendInput blocked (UIPI/secure desktop?)");
    }
}
