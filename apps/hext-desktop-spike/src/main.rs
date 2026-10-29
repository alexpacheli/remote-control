//! Spike (F4, milestone 0): can a SYSTEM service capture and control the
//! Windows secure desktop (sign-in screen, UAC prompts, Ctrl+Alt+Del)?
//! Not part of the product.
//!
//! Run as administrator:
//!
//! ```text
//! hext-desktop-spike install     installs and starts the "HextSpike" service
//! hext-desktop-spike sas         asks the service for Ctrl+Alt+Del
//! hext-desktop-spike uninstall   stops and removes it (and the policy)
//! ```
//!
//! The service launches an agent (SYSTEM) in the console session, which
//! follows the input desktop (Default, Winlogon), captures it with DXGI and
//! moves the mouse by one pixel with `SendInput`. Results go to
//! `C:\ProgramData\HextSpike`: `log.txt` and the last image of each desktop
//! (`Default.bmp`, `Winlogon.bmp`).

#[cfg(windows)]
mod windows_spike;

fn main() {
    #[cfg(windows)]
    windows_spike::main();
    #[cfg(not(windows))]
    eprintln!("só no Windows");
}
