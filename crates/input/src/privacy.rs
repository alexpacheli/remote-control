//! The privacy screen (docs/planos/F7.md): while the technician works, the
//! person at the computer sees a black screen with a notice, and their own
//! mouse and keyboard do nothing. The technician still sees and controls
//! everything.
//!
//! Windows: a black window over each monitor, left out of every capture
//! (`WDA_EXCLUDEFROMCAPTURE`, Windows 10 2004 and later) and letting clicks
//! through to what is under it; low-level hooks drop the input that did not
//! come from `SendInput`. `Ctrl+Alt+Del` cannot be blocked, and that is the
//! way out: when the computer goes to another desktop (that screen, the lock
//! screen, a UAC prompt), the privacy screen ends by itself.
//!
//! macOS (12.3 and later, see `privacy/macos.rs`): a black window over each
//! display, left out of the ScreenCaptureKit capture by its content filter;
//! an event tap drops the input that does not carry Hext's mark
//! ([`HEXT_EVENT_MARK`]). The way out is the Esc key pressed three times in
//! a row ([`EmergencyExit`]); locking the Mac or switching users ends it too.

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;

/// The privacy screen, on until dropped (or until it ends by itself: see
/// [`Privacy::ended`]).
pub struct Privacy {
    #[cfg(windows)]
    inner: imp::Running,
    #[cfg(target_os = "macos")]
    inner: macos::Running,
}

impl Privacy {
    /// Whether this computer can have it.
    pub fn available() -> bool {
        #[cfg(windows)]
        {
            imp::available()
        }
        #[cfg(target_os = "macos")]
        {
            macos::available()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            false
        }
    }

    /// Covers the screens with `text` and blocks the local mouse and
    /// keyboard. Fails rather than show anything the capture would see.
    pub fn start(text: &str) -> Result<Self, String> {
        #[cfg(windows)]
        {
            imp::Running::start(text).map(|inner| Self { inner })
        }
        #[cfg(target_os = "macos")]
        {
            macos::Running::start(text).map(|inner| Self { inner })
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = text;
            Err("a tela de privacidade não existe neste sistema".into())
        }
    }

    /// It ended by itself. Windows: someone pressed `Ctrl+Alt+Del`, the
    /// computer locked, or a UAC prompt came up. macOS: someone pressed Esc
    /// three times, or the Mac locked or went to another user.
    pub fn ended(&self) -> bool {
        #[cfg(any(windows, target_os = "macos"))]
        {
            self.inner.ended()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            true
        }
    }
}

/// What the Mac's injector writes in every event it posts
/// (`kCGEventSourceUserData`), so the privacy screen's event tap lets the
/// technician's input through and drops the rest ("HEXTPRIV" in ASCII).
pub const HEXT_EVENT_MARK: i64 = 0x4845_5854_5052_4956;

/// Whether an event's `kCGEventSourceUserData` says Hext posted it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn from_hext(user_data: i64) -> bool {
    user_data == HEXT_EVENT_MARK
}

/// The way out of the privacy screen on a Mac, where nothing like
/// `Ctrl+Alt+Del` gets past an event tap: Esc pressed [`Self::PRESSES`]
/// times, each within [`Self::GAP`] of the one before.
#[derive(Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct EmergencyExit {
    presses: u32,
    last: Option<Instant>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl EmergencyExit {
    const PRESSES: u32 = 3;
    const GAP: Duration = Duration::from_secs(1);

    /// One more Esc, `at` that moment: whether it was the last one needed.
    fn press(&mut self, at: Instant) -> bool {
        let in_a_row = self
            .last
            .is_some_and(|last| at.saturating_duration_since(last) <= Self::GAP);
        self.presses = if in_a_row { self.presses + 1 } else { 1 };
        self.last = Some(at);
        if self.presses >= Self::PRESSES {
            *self = Self::default();
            return true;
        }
        false
    }
}

/// Whether the Mac's session still lets the privacy screen stay on, from
/// what `CGSessionCopyCurrentDictionary` says: not with the screen locked,
/// nor with another user at the console (fast user switching). What it does
/// not say counts as fine.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn session_allows(screen_locked: Option<bool>, on_console: Option<bool>) -> bool {
    !screen_locked.unwrap_or(false) && on_console.unwrap_or(true)
}

#[cfg(windows)]
mod imp {
    // Win32 calls are unsafe by nature; each block is a plain API call.
    #![allow(unsafe_code)]

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use tracing::{info, warn};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DT_CALCRECT, DT_CENTER, DT_WORDBREAK,
        DeleteObject, DrawTextW, EndPaint, EnumDisplayMonitors, FillRect, GetMonitorInfoW, HDC,
        HGDIOBJ, HMONITOR, MONITORINFO, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor,
        TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetClientRect, HC_ACTION, HHOOK, HWND_TOPMOST, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
        LLMHF_INJECTED, LWA_ALPHA, MSG, MSLLHOOKSTRUCT, MsgWaitForMultipleObjects, PM_REMOVE,
        PeekMessageW, PostThreadMessageW, QS_ALLINPUT, RegisterClassW, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowDisplayAffinity,
        SetWindowPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        WDA_EXCLUDEFROMCAPTURE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_NULL, WM_PAINT, WNDCLASSW,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    /// `WDA_EXCLUDEFROMCAPTURE` came with this build (Windows 10 2004). On
    /// older ones Windows takes it as `WDA_MONITOR`, which the capture sees
    /// as black: there, no privacy screen at all.
    const FIRST_BUILD: u32 = 19041;
    /// How often to look for another desktop, other monitors, and to put
    /// the windows back on top.
    const CHECK_EVERY: Duration = Duration::from_millis(500);
    const BLACK: COLORREF = COLORREF(0);
    const WHITE: COLORREF = COLORREF(0x00FF_FFFF);
    const TEXT_HEIGHT: i32 = 32;

    /// What the windows show: the window procedure gets nothing else.
    static TEXT: Mutex<Vec<u16>> = Mutex::new(Vec::new());

    pub fn available() -> bool {
        build().is_some_and(|build| build >= FIRST_BUILD)
    }

    /// The Windows build number.
    fn build() -> Option<u32> {
        let mut text = [0u16; 32];
        let mut size = std::mem::size_of_val(&text) as u32;
        let read = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
                w!("CurrentBuildNumber"),
                RRF_RT_REG_SZ,
                None,
                Some(text.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if read.is_err() {
            return None;
        }
        String::from_utf16_lossy(&text)
            .trim_end_matches('\0')
            .parse()
            .ok()
    }

    pub struct Running {
        stop: Arc<AtomicBool>,
        ended: Arc<AtomicBool>,
        /// The thread's ID, to wake it.
        thread_id: u32,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Running {
        pub fn start(text: &str) -> Result<Self, String> {
            if !available() {
                return Err("precisa do Windows 10 2004 ou mais novo".into());
            }
            *TEXT.lock().expect("lock") = text.encode_utf16().collect();
            let stop = Arc::new(AtomicBool::new(false));
            let ended = Arc::new(AtomicBool::new(false));
            let (ready, started) = mpsc::channel();
            let thread = {
                let (stop, ended) = (stop.clone(), ended.clone());
                std::thread::Builder::new()
                    .name("hext-privacidade".into())
                    .spawn(move || run(&stop, &ended, &ready))
                    .map_err(|e| e.to_string())?
            };
            let mut running = Self {
                stop,
                ended,
                thread_id: 0,
                thread: Some(thread),
            };
            match started.recv() {
                Ok(Ok(thread_id)) => {
                    running.thread_id = thread_id;
                    Ok(running)
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err("a tela de privacidade parou ao abrir".into()),
            }
        }

        pub fn ended(&self) -> bool {
            self.ended.load(Ordering::Relaxed)
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Woken now, not at its next look around.
            if self.thread_id != 0 {
                let _ =
                    unsafe { PostThreadMessageW(self.thread_id, WM_NULL, WPARAM(0), LPARAM(0)) };
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The windows and the hooks, on this thread (hooks call back into the
    /// thread that set them, which must keep reading its messages).
    fn run(stop: &AtomicBool, ended: &AtomicBool, ready: &mpsc::Sender<Result<u32, String>>) {
        // Where the person is: windows and hooks belong to a desktop.
        hext_capture::follow_input_desktop();
        let desktop = hext_capture::input_desktop_name();
        let cover = match Cover::new() {
            Ok(cover) => cover,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let hooks = match Hooks::new() {
            Ok(hooks) => hooks,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
        info!("tela de privacidade ligada");
        let mut cover = cover;
        let mut checked = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            pump(CHECK_EVERY, stop);
            if checked.elapsed() < CHECK_EVERY {
                continue;
            }
            checked = Instant::now();
            if hext_capture::input_desktop_name() != desktop {
                info!("tela de privacidade: o computador foi para outra tela; desligando");
                ended.store(true, Ordering::Relaxed);
                break;
            }
            if monitors() != cover.monitors {
                match Cover::new() {
                    Ok(new) => cover = new,
                    Err(e) => {
                        warn!("tela de privacidade: {e}; desligando");
                        ended.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
            cover.keep_on_top();
        }
        drop(hooks);
        drop(cover);
        info!("tela de privacidade desligada");
    }

    /// Handles messages (and hook calls) for up to `time`, as they come.
    fn pump(time: Duration, stop: &AtomicBool) {
        let deadline = Instant::now() + time;
        loop {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || stop.load(Ordering::Relaxed) {
                return;
            }
            unsafe {
                MsgWaitForMultipleObjects(None, false, left.as_millis() as u32, QS_ALLINPUT);
            }
        }
    }

    /// The monitors' rectangles on the virtual desktop.
    fn monitors() -> Vec<RECT> {
        unsafe extern "system" fn each(
            monitor: HMONITOR,
            _: HDC,
            _: *mut RECT,
            list: LPARAM,
        ) -> windows::core::BOOL {
            let list = unsafe { &mut *(list.0 as *mut Vec<RECT>) };
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
                list.push(info.rcMonitor);
            }
            true.into()
        }
        let mut list: Vec<RECT> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(each),
                LPARAM(&mut list as *mut Vec<RECT> as isize),
            );
        }
        list
    }

    /// A black window over each monitor.
    struct Cover {
        monitors: Vec<RECT>,
        windows: Vec<Window>,
    }

    impl Cover {
        fn new() -> Result<Self, String> {
            let class = register_class().ok_or("sem classe de janela")?;
            let monitors = monitors();
            if monitors.is_empty() {
                return Err("nenhum monitor".into());
            }
            let windows = monitors
                .iter()
                .map(|rect| Window::new(class, rect))
                .collect::<Result<_, _>>()?;
            Ok(Self { monitors, windows })
        }

        fn keep_on_top(&self) {
            for window in &self.windows {
                window.keep_on_top();
            }
        }
    }

    fn register_class() -> Option<PCWSTR> {
        let name = w!("HextRemotePrivacy");
        let instance = unsafe { GetModuleHandleW(None) }.ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: name,
            ..Default::default()
        };
        // (Zero the second time around: already there.)
        unsafe { RegisterClassW(&class) };
        Some(name)
    }

    /// One window, destroyed when dropped.
    struct Window(HWND);

    impl Window {
        fn new(class: PCWSTR, rect: &RECT) -> Result<Self, String> {
            let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
            // Layered and transparent: clicks go through to what is under it
            // (the technician's clicks land on the programs, not on this).
            let window = unsafe {
                CreateWindowExW(
                    WS_EX_TOPMOST
                        | WS_EX_TOOLWINDOW
                        | WS_EX_NOACTIVATE
                        | WS_EX_LAYERED
                        | WS_EX_TRANSPARENT,
                    class,
                    w!("Hext Remote: tela de privacidade"),
                    WS_POPUP,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
            }
            .map_err(|e| format!("janela: {e}"))?;
            let window = Self(window);
            unsafe {
                SetLayeredWindowAttributes(window.0, COLORREF(0), 255, LWA_ALPHA)
                    .map_err(|e| format!("janela opaca: {e}"))?;
                // Before it is shown: the capture must never see it.
                SetWindowDisplayAffinity(window.0, WDA_EXCLUDEFROMCAPTURE)
                    .map_err(|e| format!("janela fora da captura: {e}"))?;
            }
            window.keep_on_top();
            Ok(window)
        }

        fn keep_on_top(&self) {
            let _ = unsafe {
                SetWindowPos(
                    self.0,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
            };
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_PAINT {
            unsafe { paint(window) };
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// Black, with the notice in the middle.
    unsafe fn paint(window: HWND) {
        let mut paint = PAINTSTRUCT::default();
        let mut area = RECT::default();
        unsafe {
            let dc = BeginPaint(window, &mut paint);
            let _ = GetClientRect(window, &mut area);
            let brush = CreateSolidBrush(BLACK);
            FillRect(dc, &area, brush);
            let _ = DeleteObject(brush.into());
            let font = CreateFontW(
                TEXT_HEIGHT,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                0,
                w!("Segoe UI"),
            );
            let old: HGDIOBJ = SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, WHITE);
            let mut text = TEXT.lock().expect("lock").clone();
            let width = (area.right - area.left) * 2 / 3;
            let mut rect = RECT {
                left: area.left + (area.right - area.left - width) / 2,
                right: area.left + (area.right - area.left + width) / 2,
                top: 0,
                bottom: 0,
            };
            DrawTextW(
                dc,
                &mut text,
                &mut rect,
                DT_CENTER | DT_WORDBREAK | DT_CALCRECT,
            );
            let height = rect.bottom - rect.top;
            rect.left = area.left + (area.right - area.left - width) / 2;
            rect.right = rect.left + width;
            rect.top = area.top + (area.bottom - area.top - height) / 2;
            rect.bottom = rect.top + height;
            DrawTextW(dc, &mut text, &mut rect, DT_CENTER | DT_WORDBREAK);
            SelectObject(dc, old);
            let _ = DeleteObject(font.into());
            let _ = EndPaint(window, &paint);
        }
    }

    /// The low-level hooks, removed when dropped.
    struct Hooks(Vec<HHOOK>);

    impl Hooks {
        fn new() -> Result<Self, String> {
            let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
            let mut hooks = Self(Vec::new());
            for (kind, procedure) in [
                (WH_KEYBOARD_LL, keyboard as HookProc),
                (WH_MOUSE_LL, mouse as HookProc),
            ] {
                let hook =
                    unsafe { SetWindowsHookExW(kind, Some(procedure), Some(instance.into()), 0) }
                        .map_err(|e| format!("bloqueio do teclado e do mouse: {e}"))?;
                hooks.0.push(hook);
            }
            Ok(hooks)
        }
    }

    impl Drop for Hooks {
        fn drop(&mut self) {
            for hook in self.0.drain(..) {
                let _ = unsafe { UnhookWindowsHookEx(hook) };
            }
        }
    }

    type HookProc = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

    /// Keys typed at the computer go nowhere; the technician's (sent with
    /// `SendInput`, so marked injected) go on.
    unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if event.flags.0 & LLKHF_INJECTED.0 == 0 {
                return LRESULT(1);
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// The same for the mouse.
    unsafe extern "system" fn mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if event.flags & LLMHF_INJECTED == 0 {
                return LRESULT(1);
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn this_windows_has_a_build_number() {
            assert!(build().is_some_and(|build| build > 10_000));
        }

        /// Covers this computer's screens and blocks its mouse and keyboard
        /// for a moment: run by hand (`--ignored`), at the computer.
        #[test]
        #[ignore]
        fn the_capture_sees_through_the_cover() {
            let frame_of = |capturer: &mut Box<dyn hext_capture::Capturer>| {
                for _ in 0..20 {
                    let _ = capturer.next_frame(Duration::from_millis(100));
                }
                hext_capture::is_black(capturer.frame().expect("frame"))
            };
            let mut capturer = hext_capture::open(0, false, 30).expect("capture");
            assert_eq!(frame_of(&mut capturer), Some(false), "a black desktop?");
            let running = Running::start("Teste da tela de privacidade").expect("start");
            std::thread::sleep(Duration::from_secs(1));
            assert_eq!(
                frame_of(&mut capturer),
                Some(false),
                "the capture saw the cover"
            );
            assert!(!running.ended());
            drop(running);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_marked_events_are_from_hext() {
        assert!(from_hext(HEXT_EVENT_MARK));
        // Real keyboards and mice leave the field at zero.
        assert!(!from_hext(0));
        assert!(!from_hext(HEXT_EVENT_MARK - 1));
    }

    #[test]
    fn three_quick_escapes_end_it() {
        let start = Instant::now();
        let mut exit = EmergencyExit::default();
        assert!(!exit.press(start));
        assert!(!exit.press(start + Duration::from_millis(300)));
        assert!(exit.press(start + Duration::from_millis(600)));
        // And it starts over.
        assert!(!exit.press(start + Duration::from_millis(700)));
    }

    #[test]
    fn slow_escapes_do_not() {
        let start = Instant::now();
        let mut exit = EmergencyExit::default();
        assert!(!exit.press(start));
        assert!(!exit.press(start + Duration::from_millis(900)));
        // Too long after the second: this one counts as the first again.
        assert!(!exit.press(start + Duration::from_millis(2500)));
        assert!(!exit.press(start + Duration::from_millis(3000)));
        assert!(exit.press(start + Duration::from_millis(3500)));
    }

    #[test]
    fn a_locked_or_switched_mac_ends_it() {
        assert!(session_allows(Some(false), Some(true)));
        assert!(session_allows(None, None));
        assert!(!session_allows(Some(true), Some(true)));
        assert!(!session_allows(None, Some(false)));
        assert!(!session_allows(Some(true), None));
    }
}
