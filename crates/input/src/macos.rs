//! Input injection with CGEvent.
//!
//! Needs the "Accessibility" permission; without it macOS silently drops
//! the events, so the injector checks and explains how to grant it.
//! Coordinates are in points on the global display space.

// CoreGraphics / ApplicationServices calls are unsafe by nature.
#![allow(unsafe_code)]

use std::collections::HashSet;
use std::time::{Duration, Instant};

use hext_proto::{InputEvent, MouseButton};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
    CGScrollEventUnit,
};

use crate::keymap::mac_keycode;
use crate::{Area, Injector};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Presses closer together than this (in time and space) count as one
/// double/triple click; macOS needs the click count spelled out.
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;

pub struct CgEventInjector {
    keys_down: HashSet<u16>,
    buttons_down: HashSet<u32>,
    position: CGPoint,
    /// Last press: button, when, where, and the click count it reached.
    last_press: Option<(u32, Instant, CGPoint, i64)>,
    click_count: i64,
    wheel_rest: (f32, f32),
}

/// Whether macOS will accept injected events; if not, explains how to allow
/// it and opens the Accessibility settings page.
pub fn preflight() -> bool {
    if unsafe { AXIsProcessTrusted() } {
        return true;
    }
    tracing::warn!(
        "sem permissão de \"Acessibilidade\": mouse e teclado remotos serão ignorados. \
         Libere o Terminal (ou o app que abriu o Hext Remote) em Ajustes do Sistema → \
         Privacidade e Segurança → Acessibilidade e reinicie o host"
    );
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
    false
}

impl Default for CgEventInjector {
    fn default() -> Self {
        preflight();
        Self {
            keys_down: HashSet::new(),
            buttons_down: HashSet::new(),
            position: CGPoint { x: 0.0, y: 0.0 },
            last_press: None,
            click_count: 1,
            wheel_rest: (0.0, 0.0),
        }
    }
}

/// CoreGraphics button number for a protocol button.
fn button_number(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

impl CgEventInjector {
    /// Modifier flags implied by the keys currently held, so shortcuts
    /// (Cmd+C, Shift+click) reach applications intact.
    fn flags(&self) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for &sc in &self.keys_down {
            flags |= match sc {
                0x2A | 0x36 => CGEventFlags::MaskShift,
                0x1D | 0xE01D => CGEventFlags::MaskControl,
                0x38 | 0xE038 => CGEventFlags::MaskAlternate,
                0xE05B | 0xE05C => CGEventFlags::MaskCommand,
                _ => CGEventFlags::empty(),
            };
        }
        flags
    }

    fn post(&self, event: Option<&CGEvent>) {
        CGEvent::set_flags(event, self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, event);
    }

    fn move_to(&mut self, point: CGPoint) {
        self.position = point;
        // While a button is held, macOS expects "dragged" events.
        let (kind, button) = if self.buttons_down.contains(&0) {
            (CGEventType::LeftMouseDragged, 0)
        } else if self.buttons_down.contains(&1) {
            (CGEventType::RightMouseDragged, 1)
        } else if let Some(&other) = self.buttons_down.iter().next() {
            (CGEventType::OtherMouseDragged, other)
        } else {
            (CGEventType::MouseMoved, 0)
        };
        let event = CGEvent::new_mouse_event(None, kind, point, CGMouseButton(button));
        self.post(event.as_deref());
    }

    fn button(&mut self, number: u32, pressed: bool) {
        let kind = match (number, pressed) {
            (0, true) => CGEventType::LeftMouseDown,
            (0, false) => CGEventType::LeftMouseUp,
            (1, true) => CGEventType::RightMouseDown,
            (1, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        if pressed {
            let now = Instant::now();
            self.click_count = match self.last_press {
                Some((last, at, where_, count))
                    if last == number
                        && now.duration_since(at) < MULTI_CLICK_TIME
                        && (where_.x - self.position.x).abs() <= MULTI_CLICK_DISTANCE
                        && (where_.y - self.position.y).abs() <= MULTI_CLICK_DISTANCE =>
                {
                    count + 1
                }
                _ => 1,
            };
            self.last_press = Some((number, now, self.position, self.click_count));
            self.buttons_down.insert(number);
        } else {
            self.buttons_down.remove(&number);
        }
        let event = CGEvent::new_mouse_event(None, kind, self.position, CGMouseButton(number));
        CGEvent::set_integer_value_field(
            event.as_deref(),
            CGEventField::MouseEventClickState,
            self.click_count,
        );
        self.post(event.as_deref());
    }

    fn key(&mut self, scancode: u16, pressed: bool) {
        let Some(keycode) = mac_keycode(scancode) else {
            return;
        };
        if pressed {
            self.keys_down.insert(scancode);
        } else {
            self.keys_down.remove(&scancode);
        }
        let event = CGEvent::new_keyboard_event(None, keycode, pressed);
        let is_modifier = matches!(
            scancode,
            0x2A | 0x36 | 0x1D | 0xE01D | 0x38 | 0xE038 | 0xE05B | 0xE05C | 0x3A
        );
        if is_modifier {
            CGEvent::set_type(event.as_deref(), CGEventType::FlagsChanged);
        }
        self.post(event.as_deref());
    }
}

impl Injector for CgEventInjector {
    fn inject(&mut self, event: &InputEvent, area: Area) {
        match *event {
            InputEvent::MouseMove { x, y } => {
                let (px, py) = area.to_pixel(x, y);
                self.move_to(CGPoint {
                    x: f64::from(px),
                    y: f64::from(py),
                });
            }
            InputEvent::MouseButton { button, pressed } => {
                self.button(button_number(button), pressed);
            }
            InputEvent::Wheel { dx, dy } => {
                // Whole lines only; the remainder carries over (trackpads).
                self.wheel_rest.0 += dx;
                self.wheel_rest.1 += dy;
                let (h, v) = (self.wheel_rest.0.trunc(), self.wheel_rest.1.trunc());
                self.wheel_rest.0 -= h;
                self.wheel_rest.1 -= v;
                if h != 0.0 || v != 0.0 {
                    // wheel1: positive scrolls up; wheel2: positive scrolls left.
                    let event = CGEvent::new_scroll_wheel_event2(
                        None,
                        CGScrollEventUnit::Line,
                        2,
                        v as i32,
                        -(h as i32),
                        0,
                    );
                    self.post(event.as_deref());
                }
            }
            InputEvent::Key { scancode, pressed } => self.key(scancode, pressed),
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn release_all(&mut self) {
        let keys: Vec<u16> = self.keys_down.iter().copied().collect();
        for sc in keys {
            self.key(sc, false);
        }
        let buttons: Vec<u32> = self.buttons_down.iter().copied().collect();
        for number in buttons {
            self.button(number, false);
        }
    }
}
