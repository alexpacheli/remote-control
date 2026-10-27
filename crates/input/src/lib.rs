//! Remote input.
//!
//! - The viewer turns window events into [`InputEvent`]s; [`keymap`] maps
//!   physical keys to PC set-1 scancodes, the wire format for keys.
//! - The host replays them with an [`Injector`]: `SendInput` on Windows, a
//!   logger elsewhere (macOS injection arrives in F2).

use hext_proto::InputEvent;

pub mod keymap;
#[cfg(windows)]
mod windows;

/// The host display the viewer is looking at, in virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Area {
    /// Maps a normalized `0.0..=1.0` position to a pixel on this area.
    pub fn to_pixel(&self, nx: f32, ny: f32) -> (i32, i32) {
        let px = self.x + (nx.clamp(0.0, 1.0) * self.width.saturating_sub(1) as f32).round() as i32;
        let py =
            self.y + (ny.clamp(0.0, 1.0) * self.height.saturating_sub(1) as f32).round() as i32;
        (px, py)
    }
}

pub trait Injector: Send {
    /// Replays one event; mouse positions are relative to `area`.
    fn inject(&mut self, event: &InputEvent, area: Area);

    /// Releases every key and button this injector is holding down.
    fn release_all(&mut self);
}

/// The injector for this OS.
pub fn injector() -> Box<dyn Injector> {
    #[cfg(windows)]
    {
        Box::new(windows::SendInputInjector::default())
    }
    #[cfg(not(windows))]
    {
        Box::new(LogInjector)
    }
}

/// Logs events instead of injecting them (hosts without injection support).
pub struct LogInjector;

impl Injector for LogInjector {
    fn inject(&mut self, event: &InputEvent, _area: Area) {
        tracing::debug!("input (not injected on this OS): {event:?}");
    }

    fn release_all(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_normalized_positions() {
        let area = Area {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(area.to_pixel(0.0, 0.0), (-1920, 0));
        assert_eq!(area.to_pixel(1.0, 1.0), (-1, 1079));
        assert_eq!(area.to_pixel(0.5, 0.5), (-960, 540));
        // Out-of-range input is clamped to the display.
        assert_eq!(area.to_pixel(-3.0, 9.0), (-1920, 1079));
    }
}
