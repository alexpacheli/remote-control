//! Following the input desktop on Windows: the user's (`Default`) or the
//! secure one (`Winlogon`: sign-in, UAC prompts, Ctrl+Alt+Del). Screen
//! capture and injected input only reach the desktop of the calling
//! thread, so the threads doing them move along. Opening the secure desktop
//! takes SYSTEM (the service's agent); elsewhere the thread just stays.
#![allow(unsafe_code)] // Win32 calls

use std::cell::RefCell;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
    GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

/// A desktop this thread opened, closed with it.
struct Opened(HDESK);

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = unsafe { CloseDesktop(self.0) };
    }
}

thread_local! {
    /// The desktop this thread moved to: `None` until the first move.
    static CURRENT: RefCell<Option<Opened>> = const { RefCell::new(None) };
}

/// Moves the calling thread to the desktop getting input now, if it is
/// another one and this process may; whether it moved. The thread must own
/// no windows (as capture and input threads do not).
pub fn follow_input_desktop() -> bool {
    let Some(input) = open_input_desktop() else {
        return false;
    };
    let Some(target) = name(input.0) else {
        return false;
    };
    // (The thread's desktop handle needs no closing.)
    let here = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
        .ok()
        .and_then(name);
    if here.is_some_and(|here| here.eq_ignore_ascii_case(&target)) {
        return false;
    }
    match unsafe { SetThreadDesktop(input.0) } {
        Ok(()) => {
            tracing::info!("desktop de entrada: {target}");
            // Kept open while the thread is on it; the one before is closed.
            CURRENT.with(|current| current.replace(Some(input)));
            true
        }
        Err(e) => {
            tracing::debug!("SetThreadDesktop({target}): {e}");
            false
        }
    }
}

/// The desktop getting input, opened; `None` without the rights to.
fn open_input_desktop() -> Option<Opened> {
    // GENERIC_ALL: to read the screen and send input there.
    unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x1000_0000),
        )
    }
    .ok()
    .map(Opened)
}

fn name(desk: HDESK) -> Option<String> {
    let mut name = [0u16; 64];
    let mut needed = 0;
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}
