//! The privacy screen on macOS.
//!
//! - A black window over each display, with the notice, above everything
//!   (screen saver level, on every Space and over full-screen apps) and
//!   letting clicks through to what is under it. The host runs inside the
//!   app (a LaunchAgent at login, ARCHITECTURE.md section 4), whose main
//!   thread runs AppKit: the windows are made and closed there, through the
//!   main queue, and nothing here waits for it for long.
//! - The capture leaves those windows out (`hext_capture::exclude_windows`:
//!   ScreenCaptureKit's content filter). They show only after every running
//!   capture confirmed it, so the technician never sees them.
//! - An event tap where input enters the window server drops every mouse
//!   and keyboard event without Hext's mark (`HEXT_EVENT_MARK`, written by
//!   the injector in `kCGEventSourceUserData`). It needs the Accessibility
//!   permission, which the host already asks for to inject input.
//!
//! macOS 12.3 or newer: ScreenCaptureKit, and with it the window exclusion,
//! comes with that version; the capture needs it anyway.
//!
//! It ends by itself when someone at the Mac presses Esc three times, when
//! the Mac locks or goes to another user, or if it can no longer cover the
//! displays; and with the process, however it ends (the windows and the tap
//! belong to it).

// AppKit, CoreGraphics and CoreFoundation calls are unsafe by nature; each
// block is a plain API call.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSScreen, NSScreenSaverWindowLevel, NSTextAlignment,
    NSTextField, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, CFString, CFType,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGGetActiveDisplayList,
    CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{
    NSOperatingSystemVersion, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
};
use tracing::{info, warn};

use super::{EmergencyExit, from_hext, session_allows};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// How often to look at the session, the displays and the tap.
const CHECK_EVERY: Duration = Duration::from_millis(250);
/// The longest wait for the app's main thread.
const MAIN_THREAD_WAIT: Duration = Duration::from_secs(3);
/// `kVK_Escape`.
const ESCAPE: i64 = 0x35;
const TEXT_SIZE: f64 = 28.0;

/// The event types the tap drops when they are not Hext's: mouse, keys,
/// modifiers, scrolling, tablets, trackpad gestures and the media keys
/// (`NSEventTypeSystemDefined`).
const BLOCKED: [u32; 25] = [
    1, 2, 3, 4, 5, 6, 7, // mouse buttons, moves and drags
    10, 11, 12, // keys and modifiers
    14, // system-defined (media, brightness, eject)
    18, 19, 20, // rotate, gesture begin and end
    22, 23, 24, // scroll wheel, tablet pointer and proximity
    25, 26, 27, // other mouse buttons
    29, 30, 31, 32, 34, // gesture, magnify, swipe, smart magnify, pressure
];

pub fn available() -> bool {
    let version = NSOperatingSystemVersion {
        majorVersion: 12,
        minorVersion: 3,
        patchVersion: 0,
    };
    // Without Accessibility the tap cannot block anything (nor could the
    // technician's input get in): not offered until it is allowed.
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(version)
        && unsafe { AXIsProcessTrusted() }
}

/// Tells one privacy screen's windows from the next one's, on the main
/// thread (an old one closing late must not close the new one's).
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub struct Running {
    stop: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    pub fn start(text: &str) -> Result<Self, String> {
        if !available() {
            return Err(
                "precisa do macOS 12.3 ou mais novo e da permissão de Acessibilidade".into(),
            );
        }
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let text = text.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::channel();
        let thread = {
            let (stop, ended) = (stop.clone(), ended.clone());
            std::thread::Builder::new()
                .name("hext-privacidade".into())
                .spawn(move || run(token, &text, &stop, &ended, &ready))
                .map_err(|e| e.to_string())?
        };
        let mut running = Self {
            stop,
            ended,
            thread: Some(thread),
        };
        match started.recv() {
            Ok(Ok(())) => Ok(running),
            Ok(Err(e)) => {
                // (It cleans up before saying so.)
                if let Some(thread) = running.thread.take() {
                    let _ = thread.join();
                }
                Err(e)
            }
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
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The tap and the checks, on this thread (the tap calls back on the run
/// loop of the thread that added it, which runs here).
fn run(
    token: u64,
    text: &str,
    stop: &AtomicBool,
    ended: &Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<(), String>>,
) {
    let mut displays = active_displays();
    if let Err(e) = cover(token, text) {
        uncover(token);
        let _ = ready.send(Err(e));
        return;
    }
    // Boxed: the tap holds its address until it is gone.
    let state = Box::new(TapState {
        ended: ended.clone(),
        exit: RefCell::new(EmergencyExit::default()),
        port: Cell::new(None),
    });
    let tap = match Tap::new(&state) {
        Ok(tap) => tap,
        Err(e) => {
            uncover(token);
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    info!("tela de privacidade ligada");
    let mut displays_changed = false;
    loop {
        // The tap's callbacks run in here.
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            CHECK_EVERY.as_secs_f64(),
            false,
        );
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if ended.load(Ordering::Relaxed) {
            info!("tela de privacidade: Esc três vezes no Mac; desligando");
            break;
        }
        let (locked, on_console) = session();
        if !session_allows(locked, on_console) {
            info!("tela de privacidade: o Mac foi bloqueado ou trocou de usuário; desligando");
            ended.store(true, Ordering::Relaxed);
            break;
        }
        // macOS turns off a tap that took too long; the callback turns it
        // back on, and this catches what it missed.
        if !CGEvent::tap_is_enabled(&tap.port) {
            CGEvent::tap_enable(&tap.port, true);
        }
        // Covered again once the displays settle (AppKit hears of a change
        // a little after CoreGraphics).
        let now = active_displays();
        if now != displays {
            displays = now;
            displays_changed = true;
        } else if displays_changed {
            displays_changed = false;
            if let Err(e) = cover(token, text) {
                warn!("tela de privacidade: {e}; desligando");
                ended.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    drop(tap);
    drop(state);
    uncover(token);
    info!("tela de privacidade desligada");
}

/// Covers every display: new windows, still transparent; out of the
/// capture; then shown, and the ones they replace closed.
fn cover(token: u64, text: &str) -> Result<(), String> {
    let text = text.to_owned();
    let ids = on_main(move |mtm| open(mtm, token, &text))??;
    hext_capture::exclude_windows(&ids).map_err(|e| format!("janela fora da captura: {e}"))?;
    on_main(move |mtm| reveal(mtm, token))
}

/// Closes the windows, waiting (bounded) for them to go, and stops leaving
/// them out of new captures.
fn uncover(token: u64) {
    if let Err(e) = on_main(move |mtm| close(mtm, token)) {
        // They go as soon as the main thread gets to it.
        warn!("tela de privacidade: {e}");
    }
    let _ = hext_capture::exclude_windows(&[]);
}

/// Runs `work` on the app's main thread and returns what it gave, or fails
/// if the main thread does not get to it in time (it still runs later).
fn on_main<R: Send + 'static>(
    work: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Result<R, String> {
    if let Some(mtm) = MainThreadMarker::new() {
        return Ok(work(mtm));
    }
    let (tx, rx) = mpsc::channel();
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        let _ = tx.send(work(mtm));
    });
    rx.recv_timeout(MAIN_THREAD_WAIT)
        .map_err(|_| "o app não respondeu a tempo".to_string())
}

/// The windows, on the main thread only.
#[derive(Default)]
struct Covers {
    token: u64,
    shown: Vec<Retained<NSWindow>>,
    /// Made, left out of the capture yet or not, and still transparent.
    coming: Vec<Retained<NSWindow>>,
}

impl Covers {
    fn close_all(&mut self) {
        for window in self.shown.drain(..).chain(self.coming.drain(..)) {
            window.close();
        }
    }
}

thread_local! {
    static COVERS: RefCell<Covers> = RefCell::new(Covers::default());
}

/// Makes a transparent window over each display; the IDs of all of this
/// screen's windows (the ones shown still need leaving out until they go).
fn open(mtm: MainThreadMarker, token: u64, text: &str) -> Result<Vec<u32>, String> {
    COVERS.with_borrow_mut(|covers| {
        if covers.token != token {
            covers.close_all();
            covers.token = token;
        }
        for window in covers.coming.drain(..) {
            window.close();
        }
        let text = NSString::from_str(text);
        for screen in NSScreen::screens(mtm).iter() {
            covers.coming.push(window(mtm, screen.frame(), &text));
        }
        if covers.coming.is_empty() {
            return Err("nenhum monitor".into());
        }
        covers
            .shown
            .iter()
            .chain(&covers.coming)
            .map(|window| {
                u32::try_from(window.windowNumber())
                    .ok()
                    .filter(|&id| id > 0)
                    .ok_or_else(|| "janela sem número".to_string())
            })
            .collect()
    })
}

/// Shows the new windows and closes the ones they replace.
fn reveal(_mtm: MainThreadMarker, token: u64) {
    COVERS.with_borrow_mut(|covers| {
        if covers.token != token {
            return;
        }
        for window in &covers.coming {
            window.setAlphaValue(1.0);
            window.orderFrontRegardless();
        }
        for window in covers.shown.drain(..) {
            window.close();
        }
        covers.shown = std::mem::take(&mut covers.coming);
    });
}

fn close(_mtm: MainThreadMarker, token: u64) {
    COVERS.with_borrow_mut(|covers| {
        if covers.token == token {
            covers.close_all();
        }
    });
}

/// A black window over `frame`, with the notice in the middle, on screen
/// but transparent until [`reveal`].
fn window(mtm: MainThreadMarker, frame: NSRect, text: &NSString) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // Kept by `Covers`: closing must not free it as well.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setBackgroundColor(Some(&NSColor::blackColor()));
    window.setOpaque(true);
    window.setHasShadow(false);
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    // The technician's clicks land on the programs under it.
    window.setIgnoresMouseEvents(true);
    window.setAlphaValue(0.0);

    let label = NSTextField::wrappingLabelWithString(text, mtm);
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setFont(Some(&NSFont::systemFontOfSize(TEXT_SIZE)));
    label.setAlignment(NSTextAlignment::Center);
    let width = (frame.size.width * 2.0 / 3.0).max(200.0);
    label.setPreferredMaxLayoutWidth(width);
    let height = label.fittingSize().height;
    label.setFrame(NSRect::new(
        NSPoint::new(
            (frame.size.width - width) / 2.0,
            (frame.size.height - height) / 2.0,
        ),
        NSSize::new(width, height),
    ));
    if let Some(content) = window.contentView() {
        content.addSubview(&label);
    }
    // On screen (transparent) so the capture's list of windows has it.
    window.orderFrontRegardless();
    window
}

/// The displays as CoreGraphics sees them, to notice a change from this
/// thread.
fn active_displays() -> Vec<(u32, [i64; 4])> {
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    // (On failure the count stays at zero: at worst, one cover too many.)
    let _ = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    ids.iter()
        .take(count as usize)
        .map(|&id| {
            let bounds = CGDisplayBounds(id);
            let rect = [
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                bounds.size.height,
            ];
            (id, rect.map(|v| v.round() as i64))
        })
        .collect()
}

/// Whether the screen is locked and whether this user is at the console.
fn session() -> (Option<bool>, Option<bool>) {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return (None, None);
    };
    (
        session_flag(&session, "CGSSessionScreenIsLocked"),
        // kCGSessionOnConsoleKey
        session_flag(&session, "kCGSSessionOnConsoleKey"),
    )
}

fn session_flag(session: &CFDictionary, key: &'static str) -> Option<bool> {
    let key = CFString::from_static_str(key);
    let value = unsafe { session.value((&*key as *const CFString).cast()) };
    let value = unsafe { value.cast::<CFType>().as_ref() }?;
    value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
}

/// What the tap's callback reaches (on the tap's thread only).
struct TapState {
    ended: Arc<AtomicBool>,
    exit: RefCell<EmergencyExit>,
    /// The tap, to turn it back on when macOS turns it off.
    port: Cell<Option<NonNull<CFMachPort>>>,
}

/// The event tap, removed when dropped.
struct Tap<'a> {
    state: &'a TapState,
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

impl<'a> Tap<'a> {
    fn new(state: &'a TapState) -> Result<Self, String> {
        let mask: CGEventMask = BLOCKED.iter().fold(0, |mask, &kind| mask | (1 << kind));
        // Where input enters the window server: before any app sees it.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(filter),
                state as *const TapState as *mut c_void,
            )
        }
        .ok_or("bloqueio do teclado e do mouse: o macOS recusou (Acessibilidade)")?;
        state.port.set(Some(NonNull::from(&*port)));
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0)
            .ok_or("bloqueio do teclado e do mouse: sem fonte de eventos")?;
        let run_loop = CFRunLoop::current().ok_or("bloqueio do teclado e do mouse: sem laço")?;
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        CGEvent::tap_enable(&port, true);
        Ok(Self {
            state,
            port,
            source,
            run_loop,
        })
    }
}

impl Drop for Tap<'_> {
    fn drop(&mut self) {
        CGEvent::tap_enable(&self.port, false);
        self.run_loop
            .remove_source(Some(&self.source), unsafe { kCFRunLoopDefaultMode });
        self.port.invalidate();
        self.state.port.set(None);
    }
}

/// Lets Hext's events through and drops the rest, counting Esc presses on
/// the way. Short and never blocking: macOS turns off a slow tap.
unsafe extern "C-unwind" fn filter(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    state: *mut c_void,
) -> *mut CGEvent {
    let state = unsafe { &*(state as *const TapState) };
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        if let Some(port) = state.port.get() {
            CGEvent::tap_enable(unsafe { port.as_ref() }, true);
        }
        return event.as_ptr();
    }
    let event_ref = unsafe { event.as_ref() };
    let user_data =
        CGEvent::integer_value_field(Some(event_ref), CGEventField::EventSourceUserData);
    // Once it ended, the person has their Mac back right away.
    if from_hext(user_data) || state.ended.load(Ordering::Relaxed) {
        return event.as_ptr();
    }
    if kind == CGEventType::KeyDown
        && CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode)
            == ESCAPE
        && CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventAutorepeat) == 0
        && state.exit.borrow_mut().press(Instant::now())
    {
        state.ended.store(true, Ordering::Relaxed);
    }
    std::ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mask_covers_mouse_keys_and_scrolling() {
        let mask: CGEventMask = BLOCKED.iter().fold(0, |mask, &kind| mask | (1 << kind));
        for kind in [
            CGEventType::LeftMouseDown,
            CGEventType::MouseMoved,
            CGEventType::KeyDown,
            CGEventType::FlagsChanged,
            CGEventType::ScrollWheel,
            CGEventType::OtherMouseDragged,
        ] {
            assert_ne!(mask & (1 << kind.0), 0, "{kind:?}");
        }
    }

    /// Blocks this Mac's mouse and keyboard for up to 10 seconds: by hand
    /// (`cargo test -p hext-input the_tap_blocks -- --ignored`), at a Mac
    /// whose terminal has the Accessibility permission. Mouse and keys do
    /// nothing; Esc three times ends it early. (The windows need the app's
    /// main thread, which a test does not have: those go by the checklist
    /// in the F7 plan, milestone 2.)
    #[test]
    #[ignore]
    fn the_tap_blocks_until_three_escapes() {
        let state = TapState {
            ended: Arc::new(AtomicBool::new(false)),
            exit: RefCell::new(EmergencyExit::default()),
            port: Cell::new(None),
        };
        let tap = Tap::new(&state).expect("tap (Accessibility?)");
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until && !state.ended.load(Ordering::Relaxed) {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.25, false);
            assert!(CGEvent::tap_is_enabled(&tap.port));
        }
        drop(tap);
    }
}
