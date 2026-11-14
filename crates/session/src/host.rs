//! Host side of a session.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_capture::virtual_display::{self, Lease};
use hext_capture::{CaptureError, Capturer, Display};
use hext_clipboard::Clipboard;
use hext_codec::EncoderSettings;
use hext_crypto::PublicKey;
use hext_crypto::pake::Verifier;
use hext_input::Area;
use hext_metrics::Gaps;
use hext_net::video::{LOG_TARGET, ResendRequest, VideoSender};
use hext_net::{Admission, Connection, ConnectionError, NetError};
use hext_proto::{
    AudioPacket, ChatText, Control, Datagram, Displays, ENDED_ON_HOST, Hello, InputEvent,
    MAX_CLIPBOARD_TEXT, PeerInfo, Permissions, Privacy, RejectReason, StreamKind, VideoFrame,
    VirtualDisplay,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::chat_pace::ChatPace;
use crate::files::{self, Direction, FileAccess, FileEnd, Stage, Tell, Transfer};
use crate::{KeptResume, Lockout, Result, ResumeTokens, SessionError, codes_match};

/// How long the viewer has to hear that the host ended the session.
const BYE_WAIT: Duration = Duration::from_secs(1);
/// How often a still screen repeats the number of its last frame.
const IDLE_NOTICE: Duration = Duration::from_millis(250);
/// How often to check whether a monitor came or went.
const DISPLAYS_CHECK: Duration = Duration::from_secs(3);
/// Sound packets waiting for the network (10 ms each): a few, then the
/// oldest are the ones to lose.
pub const AUDIO_QUEUE: usize = 8;
/// How long a sound packet may wait for room among the datagrams: later
/// than this it would only be dropped by the viewer's queue.
const AUDIO_SEND_WAIT: Duration = Duration::from_millis(30);

pub struct HostConfig {
    pub peer: PeerInfo,
    /// The code shown to the host's user, good for any number of sessions;
    /// `None`: sessions only through the server (accepted one by one).
    pub access_code: Option<String>,
    /// One-time codes given to viewers the host's user accepted (through
    /// the server); each also opens one session.
    pub session_codes: crate::SessionCodes,
    /// Where the screen comes from and the input goes.
    pub desktop: Arc<dyn Desktop>,
    /// Keeps the host's user aware of the session, with a way to end it.
    pub presence: Option<Arc<dyn Presence>>,
    /// Sessions without anyone accepting, with the password; `None`: this
    /// host has no such thing (it only exists installed).
    pub unattended: Option<UnattendedAccess>,
    /// The tokens viewers come back with.
    pub resumes: ResumeTokens,
    /// Restarts this computer; `None`: this host cannot (it takes the
    /// service, to be back afterwards).
    pub restart: Option<Arc<dyn Restart>>,
    /// Where files from viewers go; `None`: this host takes none.
    pub files: Option<Arc<dyn FileAccess>>,
    /// Who the account lets in (F5); `None`: no account here. A session
    /// let in that way ends once its viewer is no longer.
    pub owners: Option<Arc<dyn Owners>>,
}

/// Who the account bound to this computer lets in without anyone accepting.
pub trait Owners: Send + Sync {
    /// Whether `viewer` is still let in, as the latest list says.
    fn still(&self, viewer: &PublicKey) -> bool;
    /// Whether `viewer` is still let in with at least `permissions`: a
    /// session the account now lets do less ends too.
    fn still_with(&self, viewer: &PublicKey, _permissions: &Permissions) -> bool {
        self.still(viewer)
    }
}

/// How often a session let in without anyone accepting (by the account, the
/// password or as a trusted technician) checks that the way in still holds.
const GRANT_CHECK: Duration = Duration::from_secs(10);

/// The salt of the password a session came in with: a new password has
/// another one.
pub type PasswordSalt = [u8; hext_crypto::pake::SALT_LEN];

/// What a restart takes on the host.
pub trait Restart: Send + Sync {
    /// Keeps `kept` for after the restart, and restarts the computer.
    /// `who`: the viewer who asked, as the audit names it.
    fn restart(&self, kept: &KeptResume, who: &str) -> std::result::Result<(), String>;
    /// The host's user ended the session: what [`Self::restart`] kept is no
    /// longer good, even if the computer does restart.
    fn revoke(&self);
}

/// What the unattended password takes on the host.
pub struct UnattendedAccess {
    /// This computer's key: the exchange is tied to it.
    pub key: PublicKey,
    pub lockout: Lockout,
    pub password: Arc<dyn UnattendedPassword>,
    /// Where the outcomes go, on their way to the server.
    pub attempts: mpsc::UnboundedSender<Attempt>,
}

/// How a session with nobody accepting went. The server cannot see that, and
/// limits requests by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The viewer's ID, as in [`crate::Grant`].
    pub viewer: String,
    /// With the password or a token; `false`: the password was wrong.
    pub entered: bool,
}

/// Where the password is kept and what is said about its use.
pub trait UnattendedPassword: Send + Sync {
    /// What is kept of the password now; `None`: the access is off. Read on
    /// each request, so a change needs no restart.
    fn verifier(&self) -> Option<Verifier>;
    /// Writes down something that happened, for the owner of the computer.
    fn audit(&self, entry: &str);
    /// `viewer` got in with the password and asked to be remembered: from
    /// now on it is trusted without the password, until revoked or the
    /// password changes. Nothing of the password is kept. Whether it was
    /// kept (where there is no such list, it is not).
    fn bind(&self, _viewer: &PublicKey, _grant: &crate::Grant) -> bool {
        false
    }
    /// `viewer` got in with the password and asked this computer to bind to
    /// its account, chain `chain` ([`crate::Grant::account`]): from then on
    /// that account's devices come in without it, once this computer is on
    /// the account's list. Done in the background (it fetches the chain);
    /// the implementation audits the outcome. Where there is no binding (not
    /// installed, macOS), nothing happens.
    fn bind_account(&self, _viewer: &PublicKey, _chain: [u8; 32], _grant: &crate::Grant) {}
    /// Whether a session `grant` let in without anyone accepting may still
    /// go on: with the password, that it is still on and (`password`, when
    /// known) the same one; as a trusted technician, that it was not revoked
    /// (where there is no such list, it holds). Checked now and then, and
    /// before a token brings the viewer back.
    fn still(
        &self,
        _viewer: &PublicKey,
        grant: &crate::Grant,
        password: Option<&PasswordSalt>,
    ) -> bool {
        if grant.bound || !grant.needs_password {
            return true;
        }
        self.verifier()
            .is_some_and(|now| password.is_none_or(|salt| now.salt() == salt))
    }
}

/// The session going on, as the host's user is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSession {
    pub name: String,
    /// `None`: connected by IP, with the access code.
    pub id: Option<String>,
    pub permissions: Permissions,
    /// Came in with the password; nobody accepted.
    pub unattended: bool,
}

/// The host's indicator of a session (ARCHITECTURE 7.3).
pub trait Presence: Send + Sync {
    /// A session starts (before anything is shared). The session ends
    /// when the returned [`Notify`], new for each session, is notified.
    fn started(&self, session: &ActiveSession) -> Arc<Notify>;
    /// The session ended (however it did).
    fn ended(&self);
    /// The files the host's user wants to send during the session started
    /// last. Once for each session.
    fn outgoing(&self) -> Option<mpsc::UnboundedReceiver<PathBuf>> {
        None
    }
    /// How a file is going.
    fn transfer(&self, _transfer: &Transfer) {}
    /// A message from the viewer, for the host's user; whether someone is
    /// there to read it.
    fn chat(&self, _text: ChatText) -> bool {
        false
    }
    /// What the host's user writes during the session started last. Once for
    /// each session.
    fn chat_out(&self) -> Option<mpsc::UnboundedReceiver<ChatText>> {
        None
    }
    /// This computer's name now, where its user can change it while the
    /// host runs; `None`: the one in [`HostConfig::peer`].
    fn device_name(&self) -> Option<String> {
        None
    }
}

/// The session on the indicator, until dropped.
struct Shown(Arc<dyn Presence>);

impl Drop for Shown {
    fn drop(&mut self) {
        self.0.ended();
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a session's screen comes from and its input goes: this process
/// ([`LocalDesktop`]), or another one (the agent of a Windows service).
pub trait Desktop: Send + Sync {
    /// Starts streaming the screen, encoded, into `frames`. With `clipboard`,
    /// also tells it each new text copied on this computer.
    fn open(
        &self,
        frames: mpsc::Sender<VideoFrame>,
        clipboard: Option<mpsc::Sender<String>>,
    ) -> BoxFuture<'_, Result<Box<dyn Screen>>>;

    /// The video's bit rate, for pacing it out.
    fn bitrate_bps(&self) -> u32;
}

/// A screen being streamed.
pub trait Screen: Send {
    /// The viewer lost video: the next frame must be a keyframe.
    fn request_keyframe(&self);
    /// The network takes about this much video now.
    fn set_bitrate(&self, _bitrate_bps: u32) {}
    /// Replays the viewer's input.
    fn inject(&mut self, event: &InputEvent);
    /// Puts text from the viewer on this computer's clipboard.
    fn set_clipboard(&mut self, text: String);
    /// Streams display `index` from now on, with a keyframe, and the mouse
    /// goes to it. A number with no display changes nothing.
    fn select_display(&self, _index: usize) {}
    /// Turns the virtual monitor on and streams it, or turns it off (only
    /// one this session turned on). Where there is none, nothing changes.
    fn set_virtual_display(&self, _on: bool) {}
    /// The virtual monitor's next size (only one this session turned on).
    fn next_virtual_size(&self) {}
    /// Turns the privacy screen on or off. Where there is none, nothing
    /// changes.
    fn set_privacy(&self, _on: bool) {}
    /// The displays there are and the one streamed; `None`: one, and no news.
    fn displays(&self) -> Option<watch::Receiver<Displays>> {
        None
    }
    /// Where the computer's sound comes out, encoded, while it is on
    /// ([`Self::set_audio`]). Once; `None`: this screen has no sound.
    fn audio(&mut self) -> Option<mpsc::Receiver<AudioPacket>> {
        None
    }
    /// Records the computer's sound, or stops.
    fn set_audio(&self, _on: bool) {}
    /// Stops streaming, releasing any key or button still held.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// The next message of the host's user; with none to come, this waits on.
async fn next_chat(chat: &mut Option<mpsc::UnboundedReceiver<ChatText>>) -> ChatText {
    if let Some(chat) = chat
        && let Some(text) = chat.recv().await
    {
        return text;
    }
    std::future::pending().await
}

/// The next news of [`Screen::displays`]; with none to come, this waits on.
pub async fn next_displays(displays: &mut Option<watch::Receiver<Displays>>) -> Displays {
    if let Some(displays) = displays
        && displays.changed().await.is_ok()
    {
        return *displays.borrow_and_update();
    }
    std::future::pending().await
}

/// The screen of this process's desktop: capture and encoding on a thread
/// of their own, input injected here.
pub struct LocalDesktop {
    /// Display index (0 = primary).
    pub display: usize,
    /// Stream a synthetic pattern instead of the real screen.
    pub test_pattern: bool,
    pub encoder: EncoderSettings,
    /// Turn the virtual monitor on from the start (the session had asked for
    /// it before this process took over).
    pub virtual_display: bool,
}

impl Desktop for LocalDesktop {
    fn open(
        &self,
        frames: mpsc::Sender<VideoFrame>,
        clipboard: Option<mpsc::Sender<String>>,
    ) -> BoxFuture<'_, Result<Box<dyn Screen>>> {
        let settings = CaptureSettings {
            display: self.display,
            test_pattern: self.test_pattern,
            encoder: self.encoder,
            virtual_display: self.virtual_display,
        };
        Box::pin(async move {
            let shared = Arc::new(Shared {
                stop: AtomicBool::new(false),
                keyframe: AtomicBool::new(false),
                bitrate: AtomicU32::new(settings.encoder.bitrate_bps),
                area: Mutex::new(Area::default()),
                wanted: Mutex::new(None),
                virtual_wanted: Mutex::new(None),
                virtual_next: AtomicBool::new(false),
                privacy_wanted: Mutex::new(None),
                listed: Mutex::new(None),
                displays: watch::Sender::default(),
            });
            // Capture and encoding run on their own thread: both are
            // blocking, and capture objects (COM on Windows) must stay on
            // the thread that made them. A one-slot channel gives
            // backpressure: while the network is busy the thread waits, and
            // the next capture picks up the latest screen.
            let (started_tx, started_rx) = oneshot::channel();
            let thread = {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name("hext-capture".into())
                    .spawn(move || capture_loop(&settings, &shared, &frames, started_tx))
                    .map_err(|e| SessionError::Protocol(format!("capture thread: {e}")))?
            };
            let started = started_rx
                .await
                .map_err(|_| SessionError::Protocol("capture thread died".into()))?;
            started?;
            tokio::spawn(check_displays(shared.clone(), settings.test_pattern));
            let (input, input_thread) = spawn_input(shared.clone())?;
            // Off until the viewer wants it; none on the test pattern.
            let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE);
            let audio = (!settings.test_pattern)
                .then(|| hext_audio::Capture::new(audio_tx))
                .flatten();
            let audio_rx = audio.is_some().then_some(audio_rx);
            // A session goes on without it.
            let clipboard = clipboard.and_then(|changed| {
                Clipboard::watch(move |text| {
                    let _ = changed.try_send(text);
                })
                .inspect_err(|e| warn!("área de transferência: {e}"))
                .ok()
            });
            Ok(Box::new(LocalScreen {
                shared,
                clipboard,
                thread: Some(thread),
                input: Some(input),
                input_thread: Some(input_thread),
                input_failed: false,
                audio,
                audio_rx,
            }) as Box<dyn Screen>)
        })
    }

    fn bitrate_bps(&self) -> u32 {
        self.encoder.bitrate_bps
    }
}

#[derive(Clone, Copy)]
struct CaptureSettings {
    display: usize,
    test_pattern: bool,
    encoder: EncoderSettings,
    virtual_display: bool,
}

struct LocalScreen {
    shared: Arc<Shared>,
    /// Only with the permission.
    clipboard: Option<Clipboard>,
    /// Taken by `close`, which waits for it.
    thread: Option<std::thread::JoinHandle<()>>,
    /// The input thread; it releases held keys when this closes.
    input: Option<std::sync::mpsc::Sender<InputEvent>>,
    /// Taken by `close`, which waits for it (keys released).
    input_thread: Option<std::thread::JoinHandle<()>>,
    /// The input thread is gone (logged once).
    input_failed: bool,
    /// The computer's sound; `None` where it cannot be recorded.
    audio: Option<hext_audio::Capture>,
    /// Taken by `audio`.
    audio_rx: Option<mpsc::Receiver<AudioPacket>>,
}

/// Also when the session is dropped without `close` (a task cancelled):
/// never leave keys stuck down on the host (the input thread releases them
/// when its channel closes), nor the capture running.
impl Drop for LocalScreen {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Keeps the capture up to date with the monitors, for as long as it runs.
/// (Not on the capture's thread: asking the system can take a while, and the
/// video would feel it.)
async fn check_displays(shared: Arc<Shared>, test_pattern: bool) {
    loop {
        tokio::time::sleep(DISPLAYS_CHECK).await;
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let listed = tokio::task::spawn_blocking(move || hext_capture::displays(test_pattern));
        match listed.await {
            // (None at all: the computer is between two arrangements.)
            Ok(Ok(displays)) if !displays.is_empty() => {
                *shared.listed.lock().expect("lock") = Some(displays);
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => debug!("monitores: {e}"),
            Err(_) => return,
        }
    }
}

/// Injects input on a thread of its own: on Windows it follows the input
/// desktop (the sign-in screen and UAC prompts, for a SYSTEM agent), which
/// only works per thread.
fn spawn_input(
    shared: Arc<Shared>,
) -> Result<(
    std::sync::mpsc::Sender<InputEvent>,
    std::thread::JoinHandle<()>,
)> {
    /// How often to check whether the input desktop changed (not on every
    /// mouse move).
    const FOLLOW_EVERY: Duration = Duration::from_millis(200);
    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    let thread = std::thread::Builder::new()
        .name("hext-input".into())
        .spawn(move || {
            let mut injector = hext_input::injector();
            let mut followed: Option<Instant> = None;
            while let Ok(event) = rx.recv() {
                if followed.is_none_or(|t| t.elapsed() >= FOLLOW_EVERY) {
                    hext_capture::follow_input_desktop();
                    followed = Some(Instant::now());
                }
                let area = *shared.area.lock().expect("lock");
                injector.inject(&event, area);
            }
            injector.release_all();
        })
        .map_err(|e| SessionError::Protocol(format!("input thread: {e}")))?;
    Ok((tx, thread))
}

impl Screen for LocalScreen {
    fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    fn set_bitrate(&self, bitrate_bps: u32) {
        self.shared.bitrate.store(bitrate_bps, Ordering::Relaxed);
    }

    fn inject(&mut self, event: &InputEvent) {
        if *event == InputEvent::SecureAttention {
            // Takes the Windows service (this process is not it).
            debug!("Ctrl+Alt+Del: só o serviço pode enviar");
            return;
        }
        let sent = self
            .input
            .as_ref()
            .is_some_and(|input| input.send(*event).is_ok());
        if !sent && !std::mem::replace(&mut self.input_failed, true) {
            warn!("input: a thread de injeção parou; mouse e teclado não chegam mais");
        }
    }

    fn set_clipboard(&mut self, text: String) {
        if let Some(clipboard) = &self.clipboard {
            clipboard.set(text);
        }
    }

    fn select_display(&self, index: usize) {
        *self.shared.wanted.lock().expect("lock") = Some(index);
    }

    fn set_virtual_display(&self, on: bool) {
        *self.shared.virtual_wanted.lock().expect("lock") = Some(on);
    }

    fn next_virtual_size(&self) {
        self.shared.virtual_next.store(true, Ordering::Relaxed);
    }

    fn set_privacy(&self, on: bool) {
        *self.shared.privacy_wanted.lock().expect("lock") = Some(on);
    }

    fn displays(&self) -> Option<watch::Receiver<Displays>> {
        Some(self.shared.displays.subscribe())
    }

    fn audio(&mut self) -> Option<mpsc::Receiver<AudioPacket>> {
        self.audio_rx.take()
    }

    fn set_audio(&self, on: bool) {
        if let Some(audio) = &self.audio {
            audio.set_on(on);
        }
    }

    fn close(mut self: Box<Self>) -> BoxFuture<'static, ()> {
        // Releases the keys and stops the capture (`Drop`), then waits for
        // the capture to let go of the screen and the encoder.
        let thread = self.thread.take();
        let input_thread = self.input_thread.take();
        // Closing its channel makes the input thread release held keys.
        self.input = None;
        drop(self);
        Box::pin(async move {
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(input) = input_thread {
                    let _ = input.join();
                }
                if let Some(thread) = thread {
                    let _ = thread.join();
                }
            })
            .await;
        })
    }
}

/// State shared between a [`LocalScreen`] and its capture thread.
struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    /// What the encoder should aim for.
    bitrate: AtomicU32,
    area: Mutex<Area>,
    /// The display the viewer asked for last, until the capture takes it.
    wanted: Mutex<Option<usize>>,
    /// The viewer asked for the virtual monitor on, or off.
    virtual_wanted: Mutex<Option<bool>>,
    /// The viewer asked for the virtual monitor's next size.
    virtual_next: AtomicBool,
    /// The viewer asked for the privacy screen on, or off.
    privacy_wanted: Mutex<Option<bool>>,
    /// The monitors as last checked, until the capture takes them.
    listed: Mutex<Option<Vec<Display>>>,
    /// What the viewer is told.
    displays: watch::Sender<Displays>,
}

/// The monitors of this computer, and which one is being captured.
struct Layout {
    displays: Vec<Display>,
    /// `None`: that one is gone.
    current: Option<usize>,
    /// This computer can turn on a virtual monitor.
    virtual_offered: bool,
    /// The virtual monitor is on because of this session: its size changes.
    virtual_resizable: bool,
    /// The privacy screen, as the viewer is told.
    privacy: Privacy,
}

impl Layout {
    /// As the viewer sees it.
    fn view(&self) -> Displays {
        Displays {
            count: self.displays.len().min(u8::MAX.into()) as u8,
            current: self.current.unwrap_or(0).min(u8::MAX.into()) as u8,
            virtual_display: match self.virtual_index() {
                _ if !self.virtual_offered => VirtualDisplay::None,
                Some(index) => {
                    let rect = self.displays[index].rect;
                    VirtualDisplay::On {
                        index: index.min(u8::MAX.into()) as u8,
                        width: rect.width.min(u16::MAX.into()) as u16,
                        height: rect.height.min(u16::MAX.into()) as u16,
                        resizable: self.virtual_resizable,
                    }
                }
                None => VirtualDisplay::Off,
            },
            privacy: self.privacy,
            // (The session says it: the sound is not the capture's.)
            audio: false,
        }
    }

    /// Where the virtual monitor is, if it is on.
    fn virtual_index(&self) -> Option<usize> {
        self.displays.iter().position(|d| d.virtual_display)
    }

    /// The monitors changed: the one captured may have another number now,
    /// or be gone.
    fn update(&mut self, displays: Vec<Display>) {
        let current = self.current.map(|current| &self.displays[current].id);
        self.current = displays.iter().position(|d| Some(&d.id) == current);
        self.displays = displays;
    }

    /// Display `index`, unless it is the one captured already.
    fn other(&self, index: usize) -> Option<Display> {
        if self.current == Some(index) {
            return None;
        }
        self.displays.get(index).cloned()
    }
}

/// How a session with the password is told apart in the audit.
fn who(grant: &crate::Grant) -> String {
    format!("\"{}\" (ID {})", grant.name, grant.id)
}

/// Writes down the end of a session with the password, when dropped.
struct AuditEnd(Arc<dyn UnattendedPassword>, String);

impl Drop for AuditEnd {
    fn drop(&mut self) {
        self.0.audit(&format!("sessão encerrada: \"{}\"", self.1));
    }
}

/// Tells the server how a session with nobody accepting went.
fn report(access: &UnattendedAccess, grant: Option<&crate::Grant>, entered: bool) {
    if let Some(grant) = grant {
        let viewer = grant.id.clone();
        // (Gone when the host is on its way out: nothing to tell anymore.)
        let _ = access.attempts.send(Attempt { viewer, entered });
    }
}

/// Whether the password may be tried by `viewer` now, and against what.
fn admit_with_password(
    access: Option<&UnattendedAccess>,
    viewer: &PublicKey,
    grant: &crate::Grant,
) -> std::result::Result<Admission, RejectReason> {
    let access = access.ok_or(RejectReason::Denied)?;
    if let Some(wait) = access.lockout.check(viewer) {
        return Err(RejectReason::LockedOut {
            seconds: wait.as_secs().max(1).try_into().unwrap_or(u32::MAX),
        });
    }
    let verifier = access.password.verifier().ok_or(RejectReason::Denied)?;
    Ok(Admission::Password {
        permissions: grant.permissions,
        verifier,
        host: access.key,
    })
}

/// Whether the way `grant` came in without anyone accepting still holds:
/// the account still lets `viewer` in with as much, the password is the same
/// (`password`, when known), the technician still trusted. With nothing to
/// check against, it holds.
fn still_holds(
    config: &HostConfig,
    viewer: &PublicKey,
    grant: &crate::Grant,
    password: Option<&PasswordSalt>,
) -> bool {
    if grant.via.is_some() {
        return config
            .owners
            .as_ref()
            .is_none_or(|owners| owners.still_with(viewer, &grant.permissions));
    }
    if grant.needs_password || grant.bound {
        return config
            .unattended
            .as_ref()
            .is_none_or(|access| access.password.still(viewer, grant, password));
    }
    true
}

/// Runs one session on an accepted connection until the client leaves.
///
/// Returns the client's identity once the session ends normally.
pub async fn serve(conn: Connection, config: Arc<HostConfig>) -> Result<PeerInfo> {
    // The viewer proved it holds this key; a session code is only good from
    // the viewer it was issued to.
    let viewer = hext_net::remote_key(&conn);
    let mut grant: Option<crate::Grant> = None;
    let mut resumed = false;
    // The password the session came in with, to see it change.
    let mut salt: Option<PasswordSalt> = None;
    let mut peer = config.peer.clone();
    if let Some(name) = config.presence.as_ref().and_then(|p| p.device_name()) {
        peer.device_name = name;
    }
    let handshake = hext_net::host_handshake(&conn, peer, |hello: &Hello| {
        let code = config.access_code.as_deref();
        if code.is_some_and(|code| codes_match(code, &hello.access_code)) {
            return Ok(Admission::Allow(Permissions::CONTROL));
        }
        grant = config
            .session_codes
            .redeem(&hello.access_code, &viewer)
            .map(|mut grant| {
                grant.permissions.restart &= config.restart.is_some();
                grant.permissions.files &= config.files.is_some();
                grant
            });
        let granted = grant.as_ref().ok_or(RejectReason::WrongCode)?;
        if !granted.needs_password {
            return Ok(Admission::Allow(granted.permissions));
        }
        // Nobody accepted: the token of a session it had, or the password.
        let back = hello
            .resume
            .and_then(|token| config.resumes.take(&token, &viewer));
        // (Only while the way it first came in holds: a password changed or a
        // technician revoked since takes the token's worth with it.)
        // (The password once: reading it may be slow.)
        let back = back.filter(|back| {
            if back.needs_password && !back.bound && back.via.is_none() {
                salt = config
                    .unattended
                    .as_ref()
                    .and_then(|access| access.password.verifier())
                    .map(|verifier| *verifier.salt());
                salt.is_some()
            } else {
                still_holds(&config, &viewer, back, None)
            }
        });
        if let Some(back) = back {
            let permissions = back.permissions;
            grant = Some(back);
            resumed = true;
            return Ok(Admission::Allow(permissions));
        }
        let admitted = admit_with_password(config.unattended.as_ref(), &viewer, granted)?;
        if let Admission::Password { verifier, .. } = &admitted {
            salt = Some(*verifier.salt());
        }
        Ok(admitted)
    })
    .await;
    // What counts as a guess is known only now.
    let unattended = config
        .unattended
        .as_ref()
        .filter(|_| grant.as_ref().is_some_and(|grant| grant.needs_password));
    let (mut control, hello) = match (handshake, unattended) {
        (Ok(done), Some(access)) => {
            access.lockout.succeeded(&viewer);
            report(access, grant.as_ref(), true);
            // Only what the viewer asked itself, end to end, too: the server
            // could add it to a request, or replay a signed ask.
            let asked = done.1.asks;
            if let Some(grant) = grant
                .as_ref()
                .filter(|grant| grant.remember && asked.remember && !resumed)
                && access.password.bind(&viewer, grant)
            {
                access
                    .password
                    .audit(&format!("técnico vinculado: {}", who(grant)));
            }
            if let Some(grant) = grant.as_ref().filter(|_| !resumed)
                && let Some(chain) = grant.account.filter(|&chain| asked.account == Some(chain))
            {
                access.password.bind_account(&viewer, chain, grant);
            }
            done
        }
        (Err(e), Some(access)) => {
            if matches!(e, NetError::Rejected(RejectReason::WrongPassword)) {
                access.lockout.failed(&viewer);
                report(access, grant.as_ref(), false);
            }
            if let (NetError::Rejected(reason), Some(grant)) = (&e, &grant) {
                access
                    .password
                    .audit(&format!("recusado: {reason:?}, {}", who(grant)));
            }
            return Err(e.into());
        }
        (handshake, None) => handshake?,
    };
    // The viewer chose these: shown on the indicator and written in the log,
    // so nothing in them may hide or fake what is around them.
    let wants_audio = hello.audio;
    let mut client = hello.peer;
    client.device_name = crate::shown_name(&client.device_name);
    client.app_version = crate::shown_name(&client.app_version);
    // Every session the host let in is audited (a trusted technician like
    // the password; one accepted at the computer too).
    let audit = config
        .unattended
        .as_ref()
        .filter(|_| resumed || grant.is_some())
        .or(unattended)
        .map(|access| access.password.clone());
    if let (true, Some(access), Some(grant)) = (resumed, &config.unattended, &grant) {
        access
            .password
            .audit(&format!("sessão retomada: {}", who(grant)));
    }
    let returnable = grant.clone();
    // Let in without anyone accepting: checked now and then, so that leaving
    // the account, a new password or a revoked technician ends it.
    let revocable = grant
        .clone()
        .filter(|grant| grant.via.is_some() || grant.needs_password || grant.bound);
    let session = match grant {
        Some(grant) => {
            if let Some(audit) = audit.as_ref().filter(|_| !resumed) {
                let how = match (&grant.via, grant.bound) {
                    (Some(via), _) => format!(" ({via})"),
                    (None, true) => " (técnico vinculado, sem senha)".to_owned(),
                    (None, false) if grant.needs_password => String::new(),
                    (None, false) => " (aceita no computador)".to_owned(),
                };
                audit.audit(&format!("sessão iniciada{how}: {}", who(&grant)));
            }
            ActiveSession {
                unattended: grant.needs_password || grant.bound,
                name: grant.name,
                id: Some(grant.id),
                permissions: grant.permissions,
            }
        }
        None => ActiveSession {
            name: client.device_name.clone(),
            id: None,
            permissions: Permissions::CONTROL,
            unattended: false,
        },
    };
    // Written however the session ends.
    let _audit_end = audit
        .clone()
        .map(|audit| AuditEnd(audit, session.name.clone()));
    // (Not the path: right after the handshake it is usually the relay,
    // and a direct one follows; the viewer's title shows it live.)
    info!(
        "conectado: {} ({:?}, v{})",
        client.device_name, client.os, client.app_version,
    );

    // On the indicator before the first frame leaves.
    let end = config
        .presence
        .as_ref()
        .map_or_else(Default::default, |presence| presence.started(&session));
    let _shown = config.presence.clone().map(Shown);

    // One slot: backpressure from the network back to the capture.
    let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(1);
    // A few slots: a burst of copies loses the middle ones, which is fine.
    let (clipboard_tx, mut clipboard_rx) = mpsc::channel::<String>(4);
    let clipboard = session.permissions.clipboard.then_some(clipboard_tx);
    let (screen, mut displays, audio) = match config.desktop.open(frames_tx, clipboard).await {
        Ok(mut screen) => {
            // The sound goes with the picture: no permission of its own.
            // Only to a viewer that plays it (an older one would not know
            // the datagrams).
            let audio = screen.audio().filter(|_| wants_audio);
            if audio.is_some() {
                screen.set_audio(true);
            }
            let mut displays = screen.displays();
            // What there is now is news for the viewer, too.
            if let Some(displays) = &mut displays {
                displays.mark_changed();
            }
            (Arc::new(Mutex::new(Some(screen))), displays, audio)
        }
        Err(e) => {
            // What failed, for the technician to read (short: the reason
            // goes in one packet).
            let mut reason = format!("captura indisponível: {e}");
            if reason.len() > 400 {
                let end = (0..=400).rev().find(|&i| reason.is_char_boundary(i));
                reason.truncate(end.unwrap_or(0));
            }
            conn.close(1u32.into(), reason.as_bytes());
            return Err(e);
        }
    };

    // Sound goes out as datagrams of its own, beside the video's pacing and
    // FEC: small, and worthless late.
    let audio_offered = audio.is_some();
    let audio_writer = audio.map(|audio| tokio::spawn(send_audio(conn.clone(), audio)));

    // Video goes out as datagrams with FEC (see `hext_net::video`).
    let video = conn.clone();
    let bitrate = config.desktop.bitrate_bps();
    let encoding = screen.clone();
    // A few slots: a viewer asking for more than this is past saving.
    let (resend_tx, mut resend_rx) = mpsc::channel::<ResendRequest>(32);
    let mut writer = tokio::spawn(async move {
        let mut sender = VideoSender::new(bitrate);
        loop {
            tokio::select! {
                biased;
                Some(request) = resend_rx.recv() => sender.resend(&video, &request).await?,
                frame = tokio::time::timeout(IDLE_NOTICE, frames_rx.recv()) => match frame {
                    Ok(Some(frame)) => sender.send(&video, &frame, &mut resend_rx).await?,
                    Ok(None) => return Ok::<_, NetError>(()),
                    // A still screen: let the viewer check it has the last frame.
                    Err(_) => sender.send_idle(&video).await?,
                },
            }
            if let Some(bitrate) = sender.take_bitrate()
                && let Some(screen) = encoding.lock().expect("lock").as_ref()
            {
                screen.set_bitrate(bitrate);
            }
        }
    });
    // A file cut off with the session is told about after it ended.
    let session_on = Arc::new(AtomicBool::new(true));
    let (file_answers, answers) = mpsc::unbounded_channel();
    let (outcomes, mut file_outcomes) = mpsc::unbounded_channel();
    let file_end = FileEnd {
        conn: conn.clone(),
        allowed: session.permissions.files,
        access: config.files.clone(),
        paths: config
            .presence
            .as_ref()
            .and_then(|presence| presence.outgoing())
            .filter(|_| session.permissions.files)
            .unwrap_or_else(|| mpsc::unbounded_channel().1),
        answers,
        outcomes,
        tell: tell_about_files(
            config.presence.clone(),
            audit.clone(),
            session.name.clone(),
            session_on.clone(),
        ),
        confirm: None,
    };
    let mut chat_out = config
        .presence
        .as_ref()
        .and_then(|presence| presence.chat_out());
    let mut input = tokio::spawn(receive_input(
        conn.clone(),
        screen.clone(),
        session.permissions,
        file_end,
    ));

    // Through the server: this viewer can come back. (A failure to tell it
    // is the connection failing, which the loop below sees.)
    let (token_hash, displaced) = match returnable {
        Some(grant) => {
            let (token, hash, displaced) = config.resumes.open(viewer, grant);
            let _ = control.send(&Control::ResumeToken(token)).await;
            (Some(hash), displaced)
        }
        None => (None, Arc::default()),
    };
    let mut lost = false;
    // Once: the system refuses a second restart while one is under way.
    let mut restarting = false;
    let mut chat_pace = ChatPace::new();
    let mut grant_check = tokio::time::interval(GRANT_CHECK);
    // Back with a token by the account: checked at once against the latest
    // list (it may have changed while the viewer was away).
    let via_account = revocable.as_ref().is_some_and(|grant| grant.via.is_some());
    if !(resumed && via_account) {
        grant_check.tick().await;
    }

    let result = loop {
        tokio::select! {
            _ = grant_check.tick(), if revocable.is_some() => {
                if let Some(grant) = &revocable
                    && !still_holds(&config, &viewer, grant, salt.as_ref())
                {
                    let why = if grant.via.is_some() {
                        "não está mais na conta, ou perdeu permissões"
                    } else {
                        "o acesso foi revogado (senha alterada ou técnico desvinculado)"
                    };
                    info!("sessão encerrada: {why}");
                    if let Some(audit) = &audit {
                        audit.audit(&format!("sessão encerrada: {}: {why}", session.name));
                    }
                    let _ = control.send(&Control::Bye).await;
                    conn.close(0u32.into(), ENDED_ON_HOST);
                    break Ok(());
                }
            }
            msg = control.recv() => match msg {
                Ok(Control::RequestKeyframe) => {
                    if let Some(screen) = screen.lock().expect("lock").as_ref() {
                        screen.request_keyframe();
                    }
                }
                Ok(Control::ResendVideo { frame, packets }) => {
                    let _ = resend_tx.try_send(ResendRequest { frame, packets });
                }
                Ok(Control::Bye) => {
                    // Closing here is what lets the client's `bye()` return.
                    conn.close(0u32.into(), b"bye");
                    break Ok(());
                }
                Ok(Control::Restart) => {
                    restarting = restarting || restart(&config, &session, token_hash.as_ref());
                    let ok = restarting;
                    if let Err(e) = control.send(&Control::RestartAnswer { ok }).await {
                        break Err(e.into());
                    }
                }
                // Refused here: a modified viewer gets nowhere.
                Ok(Control::Clipboard { text }) => {
                    if session.permissions.clipboard && text.len() <= MAX_CLIPBOARD_TEXT
                        && let Some(screen) = screen.lock().expect("lock").as_mut()
                    {
                        screen.set_clipboard(text);
                    }
                }
                Ok(Control::FileAnswer { id, outcome }) => {
                    let _ = file_answers.send((id, outcome));
                }
                // No permission to check: whoever sees the screen sees them all.
                Ok(Control::SelectDisplay { index }) => {
                    if let Some(screen) = screen.lock().expect("lock").as_ref() {
                        screen.select_display(index.into());
                    }
                }
                // It changes the computer's monitors: it takes control.
                Ok(Control::VirtualDisplay { on }) => {
                    if session.permissions.control
                        && let Some(screen) = screen.lock().expect("lock").as_ref()
                    {
                        screen.set_virtual_display(on);
                    }
                }
                // It blocks the person at the computer: it takes control.
                Ok(Control::Privacy { on }) => {
                    if session.permissions.control
                        && let Some(screen) = screen.lock().expect("lock").as_ref()
                    {
                        screen.set_privacy(on);
                        if let Some(audit) = &audit {
                            let what = if on { "ligada" } else { "desligada" };
                            audit.audit(&format!("tela de privacidade {what}: {}", session.name));
                        }
                    }
                }
                // Muted in the viewer, or back. No permission: the sound is
                // part of seeing the screen.
                Ok(Control::Audio { on }) => {
                    if audio_offered
                        && let Some(screen) = screen.lock().expect("lock").as_ref()
                    {
                        screen.set_audio(on);
                    }
                }
                Ok(Control::NextVirtualSize) => {
                    if session.permissions.control
                        && let Some(screen) = screen.lock().expect("lock").as_ref()
                    {
                        screen.next_virtual_size();
                    }
                }
                // It takes no permission either: the host's user is who reads
                // it, and who can end the session.
                Ok(Control::Chat { text }) => {
                    if !text.is_empty() && chat_pace.allows(Instant::now()) {
                        let heard = config
                            .presence
                            .as_ref()
                            .is_some_and(|presence| presence.chat(text));
                        if !heard && let Err(e) = control.send(&Control::ChatNoOne).await {
                            break Err(e.into());
                        }
                    }
                }
                Ok(other) => debug!("mensagem inesperada: {other:?}"),
                // A peer that left is a clean end; anything else is an error.
                Err(e) if e.is_peer_closed() => break Ok(()),
                Err(e) => break Err(e.into()),
            },
            Some(text) = clipboard_rx.recv() => {
                if let Err(e) = control.send(&Control::Clipboard { text }).await {
                    break Err(e.into());
                }
            }
            Some((id, outcome)) = file_outcomes.recv() => {
                if let Err(e) = control.send(&Control::FileAnswer { id, outcome }).await {
                    break Err(e.into());
                }
            }
            text = next_chat(&mut chat_out) => {
                if let Err(e) = control.send(&Control::Chat { text }).await {
                    break Err(e.into());
                }
            }
            shown = next_displays(&mut displays) => {
                let shown = Displays { audio: audio_offered, ..shown };
                // (None yet: an agent has not told.)
                if shown.count > 0
                    && let Err(e) = control.send(&Control::Displays(shown)).await
                {
                    break Err(e.into());
                }
            }
            _ = conn.closed() => break Ok(()),
            _ = displaced.notified() => {
                info!("o técnico voltou por outra conexão");
                lost = true;
                break Ok(());
            }
            _ = end.notified() => {
                info!("sessão encerrada no host");
                if let Some(restart) = &config.restart {
                    restart.revoke();
                }
                // Reliably, unlike the closing itself: a viewer that missed
                // it would wait for a return that will not happen.
                let told = async {
                    let _ = control.send(&Control::Bye).await;
                    conn.closed().await;
                };
                let _ = tokio::time::timeout(BYE_WAIT, told).await;
                conn.close(0u32.into(), ENDED_ON_HOST);
                break Ok(());
            }
            // Without video or input the session is useless: end it.
            done = &mut writer => break task_end("vídeo", done.map(|r| r.map_err(SessionError::from))),
            done = &mut input => break task_end("input", done),
        }
    };

    session_on.store(false, Ordering::Relaxed);
    // (Asked before closing, which would hide how the connection ended.)
    lost |= matches!(
        conn.close_reason(),
        Some(ConnectionError::TimedOut | ConnectionError::Reset)
    );
    if let Some(hash) = &token_hash {
        if lost {
            config.resumes.lost(hash);
        } else {
            config.resumes.forget(hash);
        }
    }

    writer.abort();
    if let Some(audio_writer) = &audio_writer {
        audio_writer.abort();
    }
    // (A finished task was already awaited by the loop and must not be
    // polled again.)
    if !input.is_finished() {
        input.abort();
        let _ = (&mut input).await;
    }
    conn.close(0u32.into(), b"session ended");
    // Releases any keys still held and stops the capture.
    let screen = screen.lock().expect("lock").take();
    if let Some(screen) = screen {
        screen.close().await;
    }
    info!("desconectado: {}", client.device_name);
    result.map(|()| client)
}

/// Shows how each file goes while the session lasts, and writes down the
/// ones that went, for the owner of a computer nobody was at.
fn tell_about_files(
    presence: Option<Arc<dyn Presence>>,
    audit: Option<Arc<dyn UnattendedPassword>>,
    who: String,
    session_on: Arc<AtomicBool>,
) -> Tell {
    Arc::new(move |transfer: Transfer| {
        if let Some(presence) = presence
            .as_ref()
            .filter(|_| session_on.load(Ordering::Relaxed))
        {
            presence.transfer(&transfer);
        }
        if let (Some(audit), Stage::Done) = (&audit, &transfer.stage) {
            let done = match transfer.direction {
                Direction::Sending => "arquivo enviado",
                Direction::Receiving => "arquivo recebido",
            };
            audit.audit(&format!("{done}: \"{}\", \"{who}\"", transfer.name));
        }
    })
}

/// Restarts the computer for this session's viewer, if it may. The session
/// goes on until the computer goes down, and is not lost if that is called
/// off on the host.
fn restart(config: &HostConfig, session: &ActiveSession, token: Option<&crate::TokenHash>) -> bool {
    let kept = token.and_then(|hash| config.resumes.kept(hash));
    let (true, Some(restart), Some(kept)) = (session.permissions.restart, &config.restart, kept)
    else {
        warn!("pedido para reiniciar recusado: {}", session.name);
        return false;
    };
    let who = format!(
        "\"{}\" (ID {})",
        session.name,
        session.id.as_deref().unwrap_or("")
    );
    match restart.restart(&kept, &who) {
        Ok(()) => true,
        Err(e) => {
            warn!("reiniciar: {e}");
            false
        }
    }
}

/// Paces the capture to the frame-rate cap. Each deadline follows the one
/// before, not the moment the frame began, so the sleep's lateness does not
/// add up: at 60 a second the capture keeps step with a 60 Hz screen, instead
/// of drifting behind it and missing one of its frames every quarter second
/// (the stutter when scrolling, with everything "at 60").
struct Pace {
    interval: Duration,
    next: Option<Instant>,
}

impl Pace {
    fn new(max_fps: f32) -> Self {
        Self {
            interval: Duration::from_secs_f32(1.0 / max_fps.max(1.0)),
            next: None,
        }
    }

    /// Sleeps until the next frame is due.
    fn wait(&self) {
        if let Some(wait) = self
            .next
            .map(|next| next.saturating_duration_since(Instant::now()))
        {
            std::thread::sleep(wait);
        }
    }

    /// The work of a frame began at `tick`.
    fn done(&mut self, tick: Instant) {
        self.next = Some(match self.next {
            // In step: a frame after the last deadline.
            Some(next) if tick.saturating_duration_since(next) < self.interval => {
                next + self.interval
            }
            // The first frame, or the first after a still screen: from now.
            _ => tick + self.interval,
        });
    }
}

/// What the capture did in a second, for the log (`hext_video=debug`): when
/// the frame rate falls and the network was not busy, this says why.
struct CaptureReport {
    since: Instant,
    /// Images that differed from the one before.
    changed: u32,
    /// Images encoded, and those the encoder held back.
    encoded: u32,
    skipped: u32,
    /// The longest encode, and the time waiting for the network to take
    /// frames.
    longest_encode: Duration,
    network: Duration,
    /// Gaps between the encoded images while the screen moved.
    gaps: Gaps,
}

impl CaptureReport {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            changed: 0,
            encoded: 0,
            skipped: 0,
            longest_encode: Duration::ZERO,
            network: Duration::ZERO,
            gaps: Gaps::default(),
        }
    }

    fn roll(&mut self) {
        if self.since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let (gap, late) = self.gaps.take();
        debug!(
            target: LOG_TARGET,
            "captura: {} imagens novas, {} codificadas (a mais lenta em {} ms), {} seguradas pelo \
             codificador, {} ms esperando a rede, maior intervalo {} ms ({late} acima de {} ms)",
            self.changed,
            self.encoded,
            self.longest_encode.as_millis(),
            self.skipped,
            self.network.as_millis(),
            gap.as_millis(),
            Gaps::LATE.as_millis(),
        );
        // (The gaps go on from the last image.)
        let gaps = std::mem::take(&mut self.gaps);
        *self = Self {
            gaps,
            ..Self::new()
        };
    }
}

/// Opens the display `config` names. A session going on with a new agent may
/// name one that is gone: then the main one.
fn open_first(
    config: &CaptureSettings,
) -> std::result::Result<(Box<dyn Capturer>, Layout), CaptureError> {
    let displays = hext_capture::displays(config.test_pattern)?;
    let current = first_display(
        &displays,
        config.display,
        virtual_display::turned_on_by_hext(),
    );
    let display = displays
        .get(current)
        .ok_or(CaptureError::NoSuchDisplay(current))?;
    let capturer = hext_capture::open_display(display, config.test_pattern, fps(config))?;
    let current = Some(current);
    let layout = Layout {
        displays,
        current,
        virtual_offered: false,
        virtual_resizable: false,
        privacy: Privacy::None,
    };
    Ok((capturer, layout))
}

/// Which display a session starts on: the one asked for (a session going on
/// with a new agent), else the virtual monitor, if the Hext turned it on
/// (`ours`: it is there for the session, and whatever else is listed may be
/// a monitor without a cable, which the system keeps listing for a while),
/// else the main one. A virtual monitor the computer's owner keeps on is
/// one more monitor.
fn first_display(displays: &[Display], asked: usize, ours: bool) -> usize {
    if asked != 0 {
        if asked < displays.len() {
            return asked;
        }
        warn!("o monitor {} não existe", asked + 1);
    }
    displays
        .iter()
        .position(|d| ours && d.virtual_display)
        .unwrap_or(0)
}

/// Whether turning the virtual monitor off leaves something to show.
fn can_turn_off(displays: &[Display]) -> bool {
    displays.iter().any(|d| !d.virtual_display)
}

/// How long a screen must stay black before the session moves to the
/// virtual monitor.
const BLACK_CONFIRM: Duration = Duration::from_secs(3);

/// The least time between two looks at the pixels (each reads the image
/// back from the GPU on Windows).
const BLACK_LOOK: Duration = Duration::from_secs(1);

/// A monitor that is listed but shows nothing (turned off at the button,
/// a TV on another input, an output the session cannot draw on): the
/// capture works and the technician sees black. Watched in the keyframes
/// ([`hext_capture::is_black`]; not in their size, which a hardware encoder
/// pads): black, then black again [`BLACK_CONFIRM`] later, and the session
/// asks for the virtual monitor, once.
#[derive(Default)]
struct BlackWatch {
    /// When the screen was first seen black.
    since: Option<Instant>,
    /// The second look was asked for.
    asked: bool,
    /// Moved to the virtual monitor already (or tried): not again.
    done: bool,
    /// When the pixels were last looked at.
    looked: Option<Instant>,
}

/// What the watch wants after a keyframe.
#[derive(Debug, PartialEq, Eq)]
enum BlackVerdict {
    /// Nothing to do.
    Fine,
    /// Black for long enough: move to the virtual monitor.
    Black,
}

impl BlackWatch {
    /// Whether to look at this keyframe's pixels: not after the move, and
    /// not more often than [`BLACK_LOOK`] (keyframes come quickly on a
    /// lossy link), except for the second look.
    fn look(&mut self, now: Instant) -> bool {
        let second = self.asked;
        let rested = self.looked.is_none_or(|looked| now - looked >= BLACK_LOOK);
        if self.done || !(second || rested) {
            return false;
        }
        self.looked = Some(now);
        true
    }

    /// A keyframe went out; `black` says whether it was.
    fn keyframe(&mut self, black: bool, now: Instant) -> BlackVerdict {
        if self.done || !black {
            self.since = None;
            self.asked = false;
            return BlackVerdict::Fine;
        }
        match self.since {
            None => {
                self.since = Some(now);
                BlackVerdict::Fine
            }
            Some(since) if now - since >= BLACK_CONFIRM => {
                self.done = true;
                self.since = None;
                BlackVerdict::Black
            }
            Some(_) => BlackVerdict::Fine,
        }
    }

    /// Whether a keyframe is due, for the second look.
    fn due(&mut self, now: Instant) -> bool {
        match self.since {
            Some(since) if !self.asked && now - since >= BLACK_CONFIRM => {
                self.asked = true;
                true
            }
            _ => false,
        }
    }

    /// Another monitor is shown: what was seen was of the last one, and its
    /// first keyframe is looked at (a still screen sends no other).
    fn moved(&mut self) {
        self.since = None;
        self.asked = false;
        self.looked = None;
    }
}

/// What the privacy screen shows the person at the computer.
const PRIVACY_TEXT: &str = "Este computador está sendo acessado pelo suporte técnico.

A tela volta ao normal quando o 
                            técnico terminar.
Para interromper agora, aperte Ctrl+Alt+Del.";

/// The privacy screen, as the capture keeps it: on while the viewer wants it
/// and this capture runs (it goes off with the session, however it ends).
struct PrivacySwitch {
    offered: bool,
    on: Option<hext_input::privacy::Privacy>,
}

impl PrivacySwitch {
    fn new(offered: bool) -> Self {
        Self { offered, on: None }
    }

    /// Does what the viewer `asked`, and notices it ending by itself
    /// (`Ctrl+Alt+Del`). Whether the viewer should hear of it.
    fn follow(&mut self, asked: Option<bool>) -> bool {
        let mut news = asked.is_some();
        if self
            .on
            .as_ref()
            .is_some_and(hext_input::privacy::Privacy::ended)
        {
            self.on = None;
            news = true;
        }
        match asked {
            Some(true) if self.offered && self.on.is_none() => {
                match hext_input::privacy::Privacy::start(PRIVACY_TEXT) {
                    Ok(on) => self.on = Some(on),
                    Err(e) => warn!("tela de privacidade: {e}"),
                }
            }
            Some(false) => self.on = None,
            _ => {}
        }
        news
    }

    fn state(&self) -> Privacy {
        match (self.offered, &self.on) {
            (false, _) => Privacy::None,
            (true, None) => Privacy::Off,
            (true, Some(_)) => Privacy::On,
        }
    }
}

/// The virtual monitor, as the capture keeps it.
#[derive(Default)]
struct VirtualSwitch {
    /// Held while this session wants it on.
    lease: Option<Lease>,
    /// Turned on (`true`) or off, until the list of monitors shows it.
    awaiting: Option<Awaiting>,
}

struct Awaiting {
    on: bool,
    since: Instant,
    looked: Instant,
}

/// How often the list is looked at while a monitor comes or goes.
const AWAIT_LOOK: Duration = Duration::from_millis(100);

impl VirtualSwitch {
    /// The viewer asked for it on or off: the virtual monitor's number, if
    /// it is there already. Otherwise it is turned on or off, and
    /// [`Self::arrived`] says when the list shows it: the video goes on
    /// meanwhile.
    fn switch(&mut self, on: bool, layout: &Layout) -> Option<usize> {
        if !layout.virtual_offered {
            return None;
        }
        if on {
            if self.lease.is_none() {
                match virtual_display::turn_on() {
                    Ok(lease) => self.lease = Some(lease),
                    Err(e) => {
                        warn!("{e}");
                        return None;
                    }
                }
            }
            let index = layout.virtual_index();
            if index.is_none() {
                self.wait(true);
            }
            index
        } else {
            if !can_turn_off(&layout.displays) {
                info!("monitor virtual: fica ligado, é o único monitor");
                return None;
            }
            // Only one the Hext turned on goes off (and only then is there
            // anything to wait for).
            let owned = self.lease.take().is_some_and(|lease| lease.owns());
            if owned {
                self.wait(false);
            }
            None
        }
    }

    /// The virtual monitor is on because of this session.
    fn owns(&self) -> bool {
        self.lease.as_ref().is_some_and(Lease::owns)
    }

    fn wait(&mut self, on: bool) {
        let now = Instant::now();
        self.awaiting = Some(Awaiting {
            on,
            since: now,
            looked: now,
        });
    }

    /// What was turned on or off shows in the list now: the monitors are
    /// updated, and the virtual one's number given (`Some(None)`: it went).
    fn arrived(
        &mut self,
        layout: &mut Layout,
        listed: &Mutex<Option<Vec<Display>>>,
    ) -> Option<Option<usize>> {
        let awaiting = self.awaiting.as_mut()?;
        if awaiting.looked.elapsed() < AWAIT_LOOK {
            return None;
        }
        awaiting.looked = Instant::now();
        let on = awaiting.on;
        let shown = |displays: &[Display]| displays.iter().any(|d| d.virtual_display) == on;
        match hext_capture::displays(false) {
            Ok(displays) if shown(&displays) => {
                self.awaiting = None;
                layout.update(displays);
                // A list from before is no news.
                listed.lock().expect("lock").take();
                return Some(layout.virtual_index());
            }
            Ok(_) => {}
            Err(e) => debug!("monitores: {e}"),
        }
        if awaiting.since.elapsed() > WAKE_WAIT {
            warn!("monitor virtual: a lista de monitores não mudou");
            self.awaiting = None;
        }
        None
    }
}

/// The displays, once `ready` takes them (a monitor turned on or off takes a
/// moment to come or go); `None` after [`WAKE_WAIT`].
fn wait_displays(ready: impl Fn(&[Display]) -> bool) -> Option<Vec<Display>> {
    let start = Instant::now();
    loop {
        match hext_capture::displays(false) {
            Ok(displays) if ready(&displays) => return Some(displays),
            Ok(_) => {}
            Err(e) => debug!("monitores: {e}"),
        }
        if start.elapsed() > WAKE_WAIT {
            warn!("monitor virtual: a lista de monitores não mudou");
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How long opening the capture is retried while a display wakes up.
const WAKE_WAIT: Duration = Duration::from_secs(8);
/// Between tries.
const WAKE_RETRY: Duration = Duration::from_millis(250);

/// [`open_first`], retried for up to [`WAKE_WAIT`] while the display wakes:
/// a Mac whose display slept lists no display to capture until it is on.
fn open_waking(
    config: &CaptureSettings,
) -> std::result::Result<(Box<dyn Capturer>, Layout), CaptureError> {
    retry_while_waking(|| open_first(config), WAKE_WAIT, WAKE_RETRY)
}

fn retry_while_waking<T>(
    mut open: impl FnMut() -> std::result::Result<T, CaptureError>,
    wait: Duration,
    retry: Duration,
) -> std::result::Result<T, CaptureError> {
    let start = Instant::now();
    let mut told = false;
    loop {
        match open() {
            Ok(opened) => {
                if told {
                    info!("captura: aberta depois de {:?}", start.elapsed());
                }
                return Ok(opened);
            }
            // No use waiting: these do not change with the display waking.
            Err(e @ (CaptureError::PermissionDenied(_) | CaptureError::Unsupported)) => {
                return Err(e);
            }
            Err(e) if start.elapsed() + retry > wait => return Err(e),
            Err(e) => {
                if !told {
                    info!("captura: {e}; esperando a tela acordar");
                    told = true;
                }
                std::thread::sleep(retry);
            }
        }
    }
}

fn fps(config: &CaptureSettings) -> u32 {
    config.encoder.max_fps.round() as u32
}

/// The viewer asked for the virtual monitor's next size: only one this
/// session turned on changes. The list of monitors then shows the new size,
/// and the capture follows it there.
fn next_virtual_size(layout: &Layout, virtual_switch: &VirtualSwitch) {
    let Some(index) = layout.virtual_index().filter(|_| virtual_switch.owns()) else {
        return;
    };
    let display = &layout.displays[index];
    let size = virtual_display::next_size((display.rect.width, display.rect.height));
    match virtual_display::set_size(&display.id, size) {
        Ok(()) => info!("monitor virtual: {}x{}", size.0, size.1),
        Err(e) => warn!("monitor virtual: {e}"),
    }
}

/// Sends the mouse where the capturer is.
fn aim_input(shared: &Shared, capturer: &dyn Capturer) {
    let rect = capturer.rect();
    *shared.area.lock().expect("lock") = Area {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    };
}

/// Takes what changed in the monitors and what the viewer asked for, and
/// tells the viewer how that left things; whether the capture moved. If the
/// display does not open, the capture stays where it is: the encoder takes
/// whatever comes, so only the capturer changes.
fn follow_displays(
    config: &CaptureSettings,
    shared: &Shared,
    layout: &mut Layout,
    capturer: &mut Box<dyn Capturer>,
    virtual_switch: &mut VirtualSwitch,
    privacy: &mut PrivacySwitch,
) -> bool {
    let listed = shared.listed.lock().expect("lock").take();
    let mut news = listed.is_some();
    if let Some(displays) = listed {
        layout.update(displays);
    }
    let mut asked = shared.wanted.lock().expect("lock").take();
    let switch = shared.virtual_wanted.lock().expect("lock").take();
    if let Some(on) = switch {
        asked = virtual_switch.switch(on, layout).or(asked);
    }
    if let Some(arrived) = virtual_switch.arrived(layout, &shared.listed) {
        news = true;
        asked = arrived.or(asked);
    }
    if shared.virtual_next.swap(false, Ordering::Relaxed) {
        next_virtual_size(layout, virtual_switch);
    }
    layout.virtual_resizable = virtual_switch.owns();
    let privacy_asked = shared.privacy_wanted.lock().expect("lock").take();
    let privacy_news = privacy.follow(privacy_asked);
    layout.privacy = privacy.state();
    let gone = layout.current.is_none().then_some(0);
    // The same display, somewhere else on the desktop: not every capture
    // notices, and the mouse must go there.
    let shifted = layout
        .current
        .filter(|&current| news && layout.displays[current].rect != capturer.rect());
    let target = asked
        .or(gone)
        .and_then(|index| Some((index, layout.other(index)?)))
        .or_else(|| shifted.map(|index| (index, layout.displays[index].clone())));
    let mut moved = false;
    if let Some((index, display)) = target {
        match hext_capture::open_display(&display, config.test_pattern, fps(config)) {
            Ok(opened) => {
                *capturer = opened;
                aim_input(shared, capturer.as_ref());
                layout.current = Some(index);
                moved = true;
                info!("mostrando o monitor {}", index + 1);
            }
            Err(e) => {
                warn!("monitor {}: {e}", index + 1);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let view = layout.view();
    // Who asked hears the outcome, even if it is what they already had.
    shared.displays.send_if_modified(|shown| {
        let changed = *shown != view;
        *shown = view;
        changed || asked.is_some() || switch.is_some() || privacy_news
    });
    moved
}

fn capture_loop(
    config: &CaptureSettings,
    shared: &Shared,
    frames: &mpsc::Sender<VideoFrame>,
    started: oneshot::Sender<Result<()>>,
) {
    // The screen on while it is shown, and woken before capture opens: one
    // that went off for being idle has nothing to capture (Windows: black;
    // macOS: no display to open, and the session ended at once). Here, on
    // the capture thread, which follows the desktop getting input.
    let _awake = (!config.test_pattern).then(hext_capture::KeepAwake::new);
    // Before the capturer, so it goes after it: the monitor stays on while
    // it is captured.
    let virtual_offered = !config.test_pattern && virtual_display::available();
    let mut virtual_switch = VirtualSwitch::default();
    let mut privacy =
        PrivacySwitch::new(!config.test_pattern && hext_input::privacy::Privacy::available());
    // Why there is nothing to show, if there is not: said instead of
    // "display not found".
    let mut no_display = None;
    let none = !config.test_pattern && virtual_display::no_monitor();
    if none && !virtual_offered {
        no_display = Some(CaptureError::Platform(
            "nenhum monitor ligado, e este computador não tem monitor virtual".into(),
        ));
    }
    if virtual_offered {
        // Asked for in this session (an agent that took over), or nothing
        // else to show.
        if config.virtual_display || none {
            info!("ligando o monitor virtual");
            virtual_switch.lease = virtual_display::turn_on()
                .inspect_err(|e| {
                    warn!("{e}");
                    if none {
                        no_display = Some(CaptureError::Platform(e.to_string()));
                    }
                })
                .ok();
            // Before the capture opens, so it opens there (no video yet).
            if virtual_switch.lease.is_some() {
                wait_displays(|displays| displays.iter().any(|d| d.virtual_display));
            }
        }
    }
    let opened = open_waking(config)
        .map_err(|e| match (e, no_display) {
            (CaptureError::NoSuchDisplay(_), Some(why)) => why,
            (e, _) => e,
        })
        .map_err(SessionError::from)
        .and_then(|(capturer, layout)| {
            Ok((capturer, layout, hext_codec::open_encoder(config.encoder)?))
        });
    let (mut capturer, mut layout, mut encoder) = match opened {
        Ok(mut opened) => {
            opened.1.virtual_offered = virtual_offered;
            opened.1.virtual_resizable = virtual_switch.owns();
            opened.1.privacy = privacy.state();
            shared.displays.send_replace(opened.1.view());
            let _ = started.send(Ok(()));
            opened
        }
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    info!("codificador: {}", encoder.name());
    let mut hardware = encoder.is_hardware();
    let mut pace = Pace::new(config.encoder.max_fps);
    let mut seq = 0u64;
    let mut bitrate = config.encoder.bitrate_bps;
    // Rate control skipped the last image; it must still be sent.
    let mut skipped = false;
    let mut report = CaptureReport::new();
    let mut black = BlackWatch::default();

    while !shared.stop.load(Ordering::Relaxed) {
        report.roll();
        if follow_displays(
            config,
            shared,
            &mut layout,
            &mut capturer,
            &mut virtual_switch,
            &mut privacy,
        ) {
            // What the encoder held back is of the other display.
            skipped = false;
            shared.keyframe.store(true, Ordering::Relaxed);
            black.moved();
        }
        // Respect the frame-rate cap; changes accumulate in the meantime.
        pace.wait();
        let tick = Instant::now();
        // Owed work (a skipped image, a keyframe) must not wait for a change.
        let owed = skipped || shared.keyframe.load(Ordering::Relaxed);
        let timeout = if owed {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };
        let changed = match capturer.next_frame(timeout) {
            Ok(changed) => changed,
            Err(e) => {
                warn!("captura: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        aim_input(shared, capturer.as_ref());

        report.changed += u32::from(changed);
        let wanted = shared.bitrate.load(Ordering::Relaxed);
        if wanted != bitrate {
            encoder.set_bitrate(wanted);
            bitrate = wanted;
        }
        if black.due(Instant::now()) {
            shared.keyframe.store(true, Ordering::Relaxed);
        }
        let forced = shared.keyframe.swap(false, Ordering::Relaxed);
        if forced {
            encoder.force_keyframe();
        }
        // A still screen costs nothing, unless the viewer needs a keyframe
        // or the latest image was skipped.
        if !changed && !forced && !skipped {
            if encoder.pending() {
                // The last images are still inside the encoder: collect them
                // without feeding it new input.
                let started = Instant::now();
                match encoder.drain() {
                    Ok(Some(encoded)) => {
                        report.gaps.frame(Instant::now());
                        watch_black(
                            &mut black,
                            &encoded,
                            capturer.as_ref(),
                            &layout,
                            virtual_offered,
                            shared,
                        );
                        let waited = Instant::now();
                        let sent = send_frame(
                            frames,
                            &mut seq,
                            capturer.as_ref(),
                            layout.view().current,
                            encoded,
                            started.elapsed(),
                            hardware,
                        );
                        report.network += waited.elapsed();
                        if !sent {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("encode: {e}");
                        // Encode the latest image again (with the fallback encoder).
                        skipped = true;
                    }
                }
            }
            continue;
        }
        let Some(frame) = capturer.frame() else {
            // Nothing captured yet: keep a pending keyframe request alive,
            // without spinning (with one, the capture does not wait).
            if forced {
                shared.keyframe.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
            continue;
        };

        let started = Instant::now();
        let encoded = match encoder.encode(&image(frame)) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => {
                skipped = true;
                report.skipped += 1;
                pace.done(tick);
                continue;
            }
            Err(e) => {
                warn!("encode: {e}");
                // Don't drop a keyframe request: retry on the next loop
                // (owed = true keeps the timeout at zero).
                if forced {
                    shared.keyframe.store(true, Ordering::Relaxed);
                }
                continue;
            }
        };
        let encode_time = started.elapsed();
        watch_black(
            &mut black,
            &encoded,
            capturer.as_ref(),
            &layout,
            virtual_offered,
            shared,
        );
        report.encoded += 1;
        report.longest_encode = report.longest_encode.max(encode_time);
        report.gaps.frame(Instant::now());
        pace.done(tick);
        skipped = false;
        if hardware != encoder.is_hardware() {
            hardware = encoder.is_hardware();
            warn!("codificador agora: {}", encoder.name());
        }

        let waited = Instant::now();
        let sent = send_frame(
            frames,
            &mut seq,
            capturer.as_ref(),
            layout.view().current,
            encoded,
            encode_time,
            hardware,
        );
        report.network += waited.elapsed();
        if !sent {
            break;
        }
    }
}

/// Looks at a keyframe for a black screen ([`BlackWatch`]), while the session
/// shows a monitor that is not the virtual one and could move to it.
fn watch_black(
    black: &mut BlackWatch,
    encoded: &hext_codec::Encoded,
    capturer: &dyn Capturer,
    layout: &Layout,
    virtual_offered: bool,
    shared: &Shared,
) {
    let now = Instant::now();
    if !encoded.keyframe || !virtual_offered || !black.look(now) {
        return;
    }
    let Some(frame) = capturer.frame() else {
        return;
    };
    let on_virtual = layout
        .current
        .is_some_and(|current| layout.displays[current].virtual_display);
    let dark = !on_virtual && hext_capture::is_black(frame) == Some(true);
    if black.keyframe(dark, now) == BlackVerdict::Black {
        info!(
            "tela preta há {}s no monitor {}: passando para o monitor virtual",
            BLACK_CONFIRM.as_secs(),
            layout.current.map_or(0, |current| current + 1)
        );
        *shared.virtual_wanted.lock().expect("lock") = Some(true);
    }
}

/// The captured frame as encoder input.
fn image(frame: &hext_capture::Frame) -> hext_codec::Image<'_> {
    hext_codec::Image {
        width: frame.width,
        height: frame.height,
        format: match frame.format {
            hext_capture::PixelFormat::Bgra => hext_codec::PixelFormat::Bgra,
            hext_capture::PixelFormat::Nv12 => hext_codec::PixelFormat::Nv12,
        },
        pixels: gpu_texture(frame).unwrap_or(hext_codec::Pixels::Memory(&frame.data)),
    }
}

/// The frame's GPU texture, where the capture left the image on the GPU.
#[cfg(windows)]
fn gpu_texture(frame: &hext_capture::Frame) -> Option<hext_codec::Pixels<'_>> {
    frame.texture.as_ref().map(hext_codec::Pixels::D3d11)
}

#[cfg(not(windows))]
fn gpu_texture(_frame: &hext_capture::Frame) -> Option<hext_codec::Pixels<'_>> {
    None
}

/// Hands an encoded frame to the network task; `false` once the session is over.
fn send_frame(
    frames: &mpsc::Sender<VideoFrame>,
    seq: &mut u64,
    capturer: &dyn Capturer,
    display: u8,
    encoded: hext_codec::Encoded,
    encode_time: Duration,
    hardware: bool,
) -> bool {
    let Some(frame) = capturer.frame() else {
        return true;
    };
    let msg = VideoFrame {
        seq: *seq,
        display,
        width: frame.width,
        height: frame.height,
        keyframe: encoded.keyframe,
        capture_micros: micros(capturer.copy_time()),
        encode_micros: micros(encode_time),
        hardware,
        data: encoded.data,
    };
    *seq += 1;
    frames.blocking_send(msg).is_ok()
}

/// How a video or input task ending affects the session: a peer that left
/// is a normal end, anything else is an error.
fn task_end(
    what: &str,
    done: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match done {
        Ok(Ok(())) => Ok(()),
        Ok(Err(SessionError::Net(e))) if e.is_peer_closed() => Ok(()),
        Ok(Err(e)) => Err(SessionError::Protocol(format!("{what}: {e}"))),
        Err(e) => Err(SessionError::Protocol(format!("{what}: {e}"))),
    }
}

type SharedScreen = Arc<Mutex<Option<Box<dyn Screen>>>>;

/// Sends the computer's sound as datagrams until the capture or the
/// connection ends. A packet that finds no room in time is dropped: the
/// video is not pushed aside for it, nor is it worth hearing late.
async fn send_audio(conn: Connection, mut packets: mpsc::Receiver<AudioPacket>) {
    while let Some(packet) = packets.recv().await {
        let Ok(bytes) = hext_proto::encode(&Datagram::Audio(packet)) else {
            continue;
        };
        let sent = tokio::time::timeout(AUDIO_SEND_WAIT, conn.send_datagram_wait(bytes.into()));
        if let Ok(Err(e)) = sent.await {
            debug!("som: {e}");
            return;
        }
    }
}

/// Injects the client's input until the stream or connection ends; without
/// the permission to control, the input is read and dropped.
async fn receive_input(
    conn: Connection,
    screen: SharedScreen,
    permissions: Permissions,
    file_end: FileEnd,
) -> Result<()> {
    let (kind, mut stream) = hext_net::accept_uni(&conn).await?;
    if kind != StreamKind::Input {
        return Err(SessionError::Protocol(format!(
            "unexpected {kind:?} stream"
        )));
    }
    // Only now: the input stream is the first, and the next ones are these.
    // (Not stopped with the session: the connection ending ends it, and it
    // leaves no temporary file behind.)
    tokio::spawn(files::run(file_end));
    loop {
        let event: InputEvent = stream.recv().await?;
        if !permissions.control {
            continue;
        }
        if let Some(screen) = screen.lock().expect("lock").as_mut() {
            screen.inject(&event);
        }
    }
}

fn micros(d: Duration) -> u32 {
    d.as_micros().min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use hext_capture::Rect;

    use super::*;

    #[test]
    fn the_capture_keeps_step_however_late_each_frame_wakes() {
        let mut pace = Pace::new(50.0);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        pace.done(t0);
        // Each frame begins 3 ms late; the deadlines stay 20 ms apart.
        for n in 1..=10 {
            let due = pace.next.unwrap();
            assert_eq!(due, t0 + ms(20 * n));
            pace.done(due + ms(3));
        }
        // After a still screen, it counts from the frame that broke it.
        let late = pace.next.unwrap() + ms(500);
        pace.done(late);
        assert_eq!(pace.next, Some(late + ms(20)));
    }

    #[test]
    fn the_pixels_are_looked_at_now_and_then() {
        let start = Instant::now();
        let mut watch = BlackWatch::default();
        assert!(watch.look(start));
        assert!(!watch.look(start + Duration::from_millis(300)), "too soon");
        assert!(watch.look(start + BLACK_LOOK));
        watch.keyframe(true, start + BLACK_LOOK);
        assert!(watch.due(start + BLACK_LOOK + BLACK_CONFIRM));
        // The second look always happens.
        assert!(watch.look(start + BLACK_LOOK + BLACK_CONFIRM));
        watch.keyframe(true, start + BLACK_LOOK + BLACK_CONFIRM);
        assert!(!watch.look(start + BLACK_CONFIRM * 9), "moved already");
    }

    #[test]
    fn a_screen_black_twice_in_a_row_moves_to_the_virtual_monitor_once() {
        let start = Instant::now();
        let mut watch = BlackWatch::default();
        assert_eq!(watch.keyframe(true, start), BlackVerdict::Fine);
        assert!(!watch.due(start + Duration::from_secs(1)));
        // A keyframe in between (the viewer lost video) does not decide.
        assert_eq!(
            watch.keyframe(true, start + Duration::from_secs(1)),
            BlackVerdict::Fine
        );
        // Time for the second look, asked for once.
        assert!(watch.due(start + BLACK_CONFIRM));
        assert!(!watch.due(start + BLACK_CONFIRM));
        assert_eq!(
            watch.keyframe(true, start + BLACK_CONFIRM),
            BlackVerdict::Black
        );
        // Once per session.
        assert_eq!(
            watch.keyframe(true, start + BLACK_CONFIRM * 3),
            BlackVerdict::Fine
        );
        assert!(!watch.due(start + BLACK_CONFIRM * 4));
    }

    #[test]
    fn a_screen_that_lights_up_or_another_monitor_starts_over() {
        let start = Instant::now();
        let mut watch = BlackWatch::default();
        watch.keyframe(true, start);
        assert_eq!(
            watch.keyframe(false, start + Duration::from_secs(1)),
            BlackVerdict::Fine
        );
        assert!(!watch.due(start + BLACK_CONFIRM * 2), "it lit up");
        watch.keyframe(true, start);
        watch.moved();
        assert!(!watch.due(start + BLACK_CONFIRM * 2), "another monitor");
        assert_eq!(
            watch.keyframe(true, start + BLACK_CONFIRM * 2),
            BlackVerdict::Fine,
            "the first look at the new one"
        );
    }

    fn monitor(id: &str) -> Display {
        Display {
            id: id.into(),
            rect: Rect::default(),
            virtual_display: false,
        }
    }

    #[test]
    fn capture_opens_once_the_display_wakes() {
        let mut tries = 0;
        let opened = retry_while_waking(
            || {
                tries += 1;
                if tries < 3 {
                    Err(CaptureError::NoSuchDisplay(0))
                } else {
                    Ok(tries)
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
        );
        assert_eq!(opened.unwrap(), 3);
    }

    #[test]
    fn a_display_that_never_wakes_gives_up_in_time() {
        let start = Instant::now();
        let opened: std::result::Result<(), _> = retry_while_waking(
            || Err(CaptureError::Platform("sem monitor".into())),
            Duration::from_millis(50),
            Duration::from_millis(5),
        );
        assert!(matches!(opened, Err(CaptureError::Platform(_))));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn no_permission_is_not_retried() {
        let mut tries = 0;
        let opened: std::result::Result<(), _> = retry_while_waking(
            || {
                tries += 1;
                Err(CaptureError::PermissionDenied("tela".into()))
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
        );
        assert!(matches!(opened, Err(CaptureError::PermissionDenied(_))));
        assert_eq!(tries, 1);
    }

    fn layout(ids: &[&str], current: usize) -> Layout {
        Layout {
            displays: ids.iter().map(|id| monitor(id)).collect(),
            current: Some(current),
            virtual_offered: false,
            virtual_resizable: false,
            privacy: Privacy::None,
        }
    }

    fn virtual_monitor(id: &str) -> Display {
        Display {
            virtual_display: true,
            ..monitor(id)
        }
    }

    #[test]
    fn a_session_starts_on_the_virtual_monitor_the_hext_turned_on() {
        // Monitors without a cable stay listed for a while: the virtual one
        // is the one that shows something.
        let listed = [monitor("dell 1"), monitor("dell 2"), virtual_monitor("vdd")];
        assert_eq!(first_display(&listed, 0, true), 2);
        assert_eq!(
            first_display(&listed[..2], 0, true),
            0,
            "without it, the main one"
        );
        // A session going on with a new agent stays where it was.
        assert_eq!(first_display(&listed, 1, true), 1);
        assert_eq!(
            first_display(&listed, 7, true),
            2,
            "gone: as if none was asked"
        );
    }

    #[test]
    fn the_owners_virtual_monitor_is_one_more_monitor() {
        // On by the computer's owner (for something else): the main one.
        let listed = [monitor("dell 1"), monitor("dell 2"), virtual_monitor("vdd")];
        assert_eq!(first_display(&listed, 0, false), 0);
        assert_eq!(first_display(&listed, 2, false), 2, "unless asked for");
    }

    #[test]
    fn the_viewer_hears_whether_there_is_a_virtual_monitor() {
        let mut layout = layout(&["principal"], 0);
        assert_eq!(layout.view().virtual_display, VirtualDisplay::None);
        layout.virtual_offered = true;
        assert_eq!(layout.view().virtual_display, VirtualDisplay::Off);
        layout.update(vec![monitor("principal"), virtual_monitor("vdd")]);
        assert_eq!(
            layout.view(),
            Displays {
                count: 2,
                current: 0,
                virtual_display: VirtualDisplay::On {
                    index: 1,
                    width: 0,
                    height: 0,
                    resizable: false,
                },
                privacy: Privacy::None,
                audio: false,
            }
        );
    }

    #[test]
    fn a_host_without_the_privacy_screen_says_so_when_asked() {
        let mut privacy = PrivacySwitch::new(false);
        assert_eq!(privacy.state(), Privacy::None);
        assert!(!privacy.follow(None), "nothing asked, nothing to tell");
        // The viewer hears back, though nothing turned on.
        assert!(privacy.follow(Some(true)));
        assert_eq!(privacy.state(), Privacy::None);
        assert!(privacy.on.is_none());
    }

    #[test]
    fn nothing_turns_on_where_there_is_no_virtual_monitor() {
        let layout = layout(&["principal"], 0);
        let mut switch = VirtualSwitch::default();
        assert_eq!(switch.switch(true, &layout), None);
        assert!(switch.lease.is_none() && switch.awaiting.is_none());
    }

    #[test]
    fn the_only_monitor_stays_on() {
        assert!(!can_turn_off(&[virtual_monitor("vdd")]));
        assert!(can_turn_off(&[virtual_monitor("vdd"), monitor("dell")]));
        assert!(!can_turn_off(&[]), "nothing to show either way");
    }

    #[test]
    fn off_waits_only_for_what_the_hext_turned_on() {
        // No lease (the owner's monitor, or never turned on here): nothing
        // goes off, and the video does not wait for it.
        let mut layout = layout(&["principal"], 0);
        layout.displays.push(virtual_monitor("vdd"));
        layout.virtual_offered = true;
        let mut switch = VirtualSwitch::default();
        assert_eq!(switch.switch(false, &layout), None);
        assert!(switch.awaiting.is_none());
    }

    #[test]
    fn the_monitor_on_screen_keeps_its_place_when_another_arrives() {
        let mut layout = layout(&["principal", "direita"], 1);
        layout.update(vec![
            monitor("principal"),
            monitor("esquerda"),
            monitor("direita"),
        ]);
        let view = Displays {
            count: 3,
            current: 2,
            ..Default::default()
        };
        assert_eq!(layout.view(), view);
        assert_eq!(layout.other(2), None, "already there");
    }

    #[test]
    fn a_monitor_that_goes_sends_the_capture_to_the_main_one() {
        let mut layout = layout(&["principal", "direita"], 1);
        layout.update(vec![monitor("principal")]);
        assert_eq!(layout.current, None);
        assert_eq!(layout.other(0), Some(monitor("principal")));
        let view = Displays {
            count: 1,
            current: 0,
            ..Default::default()
        };
        assert_eq!(layout.view(), view);
    }

    #[test]
    fn a_number_with_no_monitor_changes_nothing() {
        let layout = layout(&["principal", "direita"], 0);
        assert_eq!(layout.other(2), None);
        assert_eq!(layout.other(0), None);
        assert_eq!(layout.other(1), Some(monitor("direita")));
    }

    fn settings() -> CaptureSettings {
        CaptureSettings {
            display: 0,
            test_pattern: true,
            encoder: EncoderSettings::default(),
            virtual_display: false,
        }
    }

    /// Capture and encoding on their thread, on the test pattern's displays,
    /// until `stop`.
    fn capture(frames: mpsc::Sender<VideoFrame>) -> Arc<Shared> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            keyframe: AtomicBool::new(false),
            bitrate: AtomicU32::new(settings().encoder.bitrate_bps),
            area: Mutex::new(Area::default()),
            wanted: Mutex::new(None),
            virtual_wanted: Mutex::new(None),
            virtual_next: AtomicBool::new(false),
            privacy_wanted: Mutex::new(None),
            listed: Mutex::new(None),
            displays: watch::Sender::default(),
        });
        let running = shared.clone();
        std::thread::spawn(move || {
            capture_loop(&settings(), &running, &frames, oneshot::channel().0);
        });
        shared
    }

    #[test]
    fn the_mouse_goes_with_the_picture_to_the_other_monitor() {
        let (frames_tx, mut frames) = mpsc::channel(1);
        let shared = capture(frames_tx);
        let first = frames.blocking_recv().expect("a frame");
        assert_eq!((first.width, first.height), (1280, 720));

        *shared.wanted.lock().unwrap() = Some(1);
        let frame = loop {
            let frame = frames.blocking_recv().expect("a frame");
            if frame.width == 640 {
                break frame;
            }
        };
        assert!(frame.keyframe, "the other monitor starts with a keyframe");
        assert_eq!(frame.display, 1, "and says which it is");
        let view = Displays {
            count: 2,
            current: 1,
            ..Default::default()
        };
        assert_eq!(*shared.displays.borrow(), view);
        // Left of the main monitor: nowhere near where the mouse was going.
        let area = *shared.area.lock().unwrap();
        assert_eq!(area.to_pixel(0.0, 0.0), (-640, 0));
        assert_eq!(area.to_pixel(1.0, 1.0), (-1, 359));

        // What the viewer asks for and does not exist leaves it there, and
        // the viewer hears it.
        let mut told = shared.displays.subscribe();
        *shared.wanted.lock().unwrap() = Some(7);
        while told.has_changed().is_ok_and(|changed| !changed) {
            let _ = frames.try_recv();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*told.borrow_and_update(), view);
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_monitor_moved_on_the_desktop_takes_the_mouse_with_it() {
        let (frames_tx, mut frames) = mpsc::channel(1);
        let shared = capture(frames_tx);
        frames.blocking_recv().expect("a frame");

        let mut moved = hext_capture::displays(true).unwrap();
        moved[0].rect.y = 300;
        *shared.listed.lock().unwrap() = Some(moved);
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.area.lock().unwrap().y != 300 {
            assert!(Instant::now() < deadline, "the mouse stayed behind");
            let _ = frames.try_recv();
            std::thread::sleep(Duration::from_millis(5));
        }
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_monitor_that_goes_brings_the_picture_back_to_the_main_one() {
        let (frames_tx, mut frames) = mpsc::channel(1);
        let shared = capture(frames_tx);
        *shared.wanted.lock().unwrap() = Some(1);
        while frames.blocking_recv().expect("a frame").width != 640 {}

        let main = hext_capture::displays(true).unwrap().remove(0);
        *shared.listed.lock().unwrap() = Some(vec![main]);
        let frame = loop {
            let frame = frames.blocking_recv().expect("a frame");
            if frame.width == 1280 {
                break frame;
            }
        };
        assert!(frame.keyframe);
        let view = Displays {
            count: 1,
            current: 0,
            ..Default::default()
        };
        assert_eq!(*shared.displays.borrow(), view);
        shared.stop.store(true, Ordering::Relaxed);
    }
}
