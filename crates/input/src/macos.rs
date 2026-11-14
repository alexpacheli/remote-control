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
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGScrollEventUnit,
};

use crate::keymap::{mac_keycode, mac_numeric_pad};
use crate::{Area, Injector};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Presses closer together than this (in time and space) count as one
/// double/triple click; macOS needs the click count spelled out.
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;

/// Scroll distance of one wheel notch, sent in pixels so trackpad scrolling
/// stays smooth (about three lines, like Windows).
const PIXELS_PER_NOTCH: f32 = 40.0;

const CAPS_LOCK: u16 = 0x3A;

/// Each modifier flag and the Mac key (`kVK_*`) that lets go of it.
const MODIFIER_KEYS: [(CGEventFlags, u16); 5] = [
    (CGEventFlags::MaskShift, 0x38),
    (CGEventFlags::MaskControl, 0x3B),
    (CGEventFlags::MaskAlternate, 0x3A),
    (CGEventFlags::MaskCommand, 0x37),
    (CGEventFlags::MaskSecondaryFn, 0x3F), // Fn / Globe
];

/// The modifiers macOS believes held (`system`) that nobody here holds
/// (`held`), each with the key that releases it.
fn stray_modifiers(system: CGEventFlags, held: CGEventFlags) -> Vec<(CGEventFlags, u16)> {
    MODIFIER_KEYS
        .into_iter()
        .filter(|&(flag, _)| system.contains(flag) && !held.contains(flag))
        .collect()
}

pub struct CgEventInjector {
    keys_down: HashSet<u16>,
    buttons_down: HashSet<u32>,
    position: CGPoint,
    /// Last press: button, when, where, and the click count it reached.
    last_press: Option<(u32, Instant, CGPoint, i64)>,
    click_count: i64,
    /// Caps Lock is a toggle: its state lives here, not in `keys_down`.
    caps_lock: bool,
    /// Sub-pixel scroll not yet sent.
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
        // The host already asked at startup; here only a reminder per session.
        if !unsafe { AXIsProcessTrusted() } {
            tracing::warn!("sem permissão de \"Acessibilidade\": o input remoto será ignorado");
        }
        let injector = Self {
            keys_down: HashSet::new(),
            buttons_down: HashSet::new(),
            position: CGPoint { x: 0.0, y: 0.0 },
            last_press: None,
            click_count: 1,
            caps_lock: false,
            wheel_rest: (0.0, 0.0),
        };
        // A modifier left behind earlier (another session, an older
        // version) would ruin this one's typing from the first key.
        injector.release_stray_modifiers();
        injector
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
        let mut flags = if self.caps_lock {
            CGEventFlags::MaskAlphaShift
        } else {
            CGEventFlags::empty()
        };
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

    /// Posts `event` with exactly the held modifiers plus `own`, the flags
    /// of the key itself. Nothing comes from the flags the event was created
    /// with: those can carry the system's current state (a stuck Fn, say),
    /// and posting them again would keep it stuck for good.
    ///
    /// Every event carries Hext's mark in `kCGEventSourceUserData`, a field
    /// that exists for the poster's own use and goes along with the event
    /// through every event tap; the privacy screen's tap lets marked events
    /// through and drops the rest. Real keyboards and mice leave it at zero.
    /// (Telling them apart by source state or process ID would depend on
    /// how macOS fills those fields for events made with no source; this
    /// one is only ever what Hext wrote.)
    fn post(&self, event: Option<&CGEvent>, own: CGEventFlags) {
        CGEvent::set_integer_value_field(
            event,
            CGEventField::EventSourceUserData,
            crate::privacy::HEXT_EVENT_MARK,
        );
        CGEvent::set_flags(event, own | self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, event);
    }

    /// Lets go of every modifier macOS still believes held that this
    /// injector does not hold: a Fn/Globe, Command or Control left behind
    /// turns plain typing into shortcuts (the emoji picker on "é", new
    /// windows, pop-ups). Real keys held right now count as held there too,
    /// so this runs only when the remote side lets go of everything.
    fn release_stray_modifiers(&self) {
        let system = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState);
        let held = self.flags();
        for (flag, keycode) in stray_modifiers(system, held) {
            tracing::warn!("modificador preso no Mac ({flag:?}); soltando");
            let event = CGEvent::new_keyboard_event(None, keycode, false);
            CGEvent::set_type(event.as_deref(), CGEventType::FlagsChanged);
            self.post(event.as_deref(), CGEventFlags::empty());
        }
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
        self.post(event.as_deref(), CGEventFlags::empty());
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
        self.post(event.as_deref(), CGEventFlags::empty());
    }

    fn key(&mut self, scancode: u16, pressed: bool) {
        let Some(keycode) = mac_keycode(scancode) else {
            return;
        };
        if scancode == CAPS_LOCK {
            if pressed {
                self.caps_lock = !self.caps_lock;
            }
        } else if pressed {
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
        let own = if mac_numeric_pad(keycode) {
            CGEventFlags::MaskNumericPad
        } else {
            CGEventFlags::empty()
        };
        self.post(event.as_deref(), own);
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
                if !dx.is_finite() || !dy.is_finite() {
                    return;
                }
                // Whole pixels; the remainder carries over.
                self.wheel_rest.0 += dx * PIXELS_PER_NOTCH;
                self.wheel_rest.1 += dy * PIXELS_PER_NOTCH;
                let (h, v) = (self.wheel_rest.0.trunc(), self.wheel_rest.1.trunc());
                self.wheel_rest.0 -= h;
                self.wheel_rest.1 -= v;
                if h != 0.0 || v != 0.0 {
                    // wheel1: positive scrolls up; wheel2: positive scrolls left.
                    let event = CGEvent::new_scroll_wheel_event2(
                        None,
                        CGScrollEventUnit::Pixel,
                        2,
                        v as i32,
                        -(h as i32),
                        0,
                    );
                    self.post(event.as_deref(), CGEventFlags::empty());
                }
            }
            InputEvent::Key { scancode, pressed } => self.key(scancode, pressed),
            InputEvent::ReleaseAll => self.release_all(),
            // Windows only, and sent by the service, not injected.
            InputEvent::SecureAttention => {}
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
        // Whatever macOS still believes held after that was not ours to
        // hold: let go of it too (focus lost, session over, reconnecting).
        self.release_stray_modifiers();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stray_modifiers_are_the_ones_nobody_holds() {
        let system = CGEventFlags::MaskSecondaryFn
            | CGEventFlags::MaskCommand
            | CGEventFlags::MaskShift
            | CGEventFlags::MaskAlphaShift
            | CGEventFlags::MaskNumericPad;
        let held = CGEventFlags::MaskShift;
        let keys: Vec<u16> = stray_modifiers(system, held)
            .into_iter()
            .map(|(_, key)| key)
            .collect();
        // Command and Fn/Globe; never Caps Lock (a toggle) or held Shift.
        assert_eq!(keys, vec![0x37, 0x3F]);
        assert!(stray_modifiers(held, held).is_empty());
        assert!(stray_modifiers(CGEventFlags::empty(), CGEventFlags::empty()).is_empty());
    }
}
