//! What the host's user decides holds on the host: the permissions granted
//! and the button that ends a session.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hext_codec::DecoderPreference;
use hext_crypto::DeviceKey;
use hext_net::EndpointConfig;
use hext_proto::{
    ChatText, Displays, Hello, InputEvent, MAX_CLIPBOARD_TEXT, PeerInfo, Permissions, VideoFrame,
};
use hext_session::host::{ActiveSession, BoxFuture, Desktop, HostConfig, Presence, Screen};
use hext_session::{Grant, SessionCodes, client, host};
use tokio::sync::{Notify, mpsc, watch};

/// A desktop that only counts the input reaching it.
struct Counting {
    injected: Arc<AtomicU32>,
    /// Where to tell what is copied here, if the session may know.
    copied: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pasted: Arc<Mutex<Vec<String>>>,
    /// Two displays: the first shown.
    displays: watch::Sender<Displays>,
}

impl Default for Counting {
    fn default() -> Self {
        Self {
            injected: Arc::default(),
            copied: Arc::default(),
            pasted: Arc::default(),
            displays: watch::Sender::new(Displays {
                count: 2,
                current: 0,
                ..Default::default()
            }),
        }
    }
}

impl Desktop for Counting {
    fn open(
        &self,
        frames: mpsc::Sender<VideoFrame>,
        clipboard: Option<mpsc::Sender<String>>,
    ) -> BoxFuture<'_, hext_session::Result<Box<dyn Screen>>> {
        Box::pin(async move {
            *self.copied.lock().unwrap() = clipboard;
            Ok(Box::new(CountingScreen {
                _frames: frames,
                injected: self.injected.clone(),
                pasted: self.pasted.clone(),
                displays: self.displays.clone(),
            }) as Box<dyn Screen>)
        })
    }

    fn bitrate_bps(&self) -> u32 {
        1_000_000
    }
}

struct CountingScreen {
    /// Open while the session runs: a closed one ends it.
    _frames: mpsc::Sender<VideoFrame>,
    injected: Arc<AtomicU32>,
    pasted: Arc<Mutex<Vec<String>>>,
    displays: watch::Sender<Displays>,
}

impl Screen for CountingScreen {
    fn request_keyframe(&self) {}

    fn inject(&mut self, _event: &InputEvent) {
        self.injected.fetch_add(1, Ordering::SeqCst);
    }

    fn set_clipboard(&mut self, text: String) {
        self.pasted.lock().unwrap().push(text);
    }

    fn select_display(&self, index: usize) {
        self.displays.send_modify(|displays| {
            if index < displays.count.into() {
                displays.current = index as u8;
            }
        });
    }

    fn displays(&self) -> Option<watch::Receiver<Displays>> {
        Some(self.displays.subscribe())
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// An indicator keeping what it was told.
struct Indicator {
    /// What the user read of the chat.
    read: Mutex<Vec<ChatText>>,
    chat_in: Notify,
    /// Nobody at this computer.
    nobody: AtomicBool,
    chat_tx: mpsc::UnboundedSender<ChatText>,
    chat_rx: Mutex<Option<mpsc::UnboundedReceiver<ChatText>>>,
    shown: Mutex<Option<ActiveSession>>,
    end: Mutex<Option<Arc<Notify>>>,
    ended: AtomicU32,
}

impl Default for Indicator {
    fn default() -> Self {
        let (chat_tx, chat_rx) = mpsc::unbounded_channel();
        Self {
            read: Mutex::default(),
            chat_in: Notify::new(),
            nobody: AtomicBool::new(false),
            chat_tx,
            chat_rx: Mutex::new(Some(chat_rx)),
            shown: Mutex::default(),
            end: Mutex::default(),
            ended: AtomicU32::default(),
        }
    }
}

impl Presence for Indicator {
    fn chat(&self, text: ChatText) -> bool {
        if self.nobody.load(Ordering::SeqCst) {
            return false;
        }
        self.read.lock().unwrap().push(text);
        self.chat_in.notify_one();
        true
    }

    fn chat_out(&self) -> Option<mpsc::UnboundedReceiver<ChatText>> {
        self.chat_rx.lock().unwrap().take()
    }

    fn started(&self, session: &ActiveSession) -> Arc<Notify> {
        *self.shown.lock().unwrap() = Some(session.clone());
        let end = Arc::new(Notify::new());
        *self.end.lock().unwrap() = Some(end.clone());
        end
    }

    fn ended(&self) {
        self.ended.fetch_add(1, Ordering::SeqCst);
    }
}

struct Running {
    session: client::ClientHandle,
    displays: watch::Sender<Displays>,
    injected: Arc<AtomicU32>,
    copied: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pasted: Arc<Mutex<Vec<String>>>,
    indicator: Arc<Indicator>,
    host: tokio::task::JoinHandle<hext_session::Result<PeerInfo>>,
    _endpoints: (hext_net::Endpoint, hext_net::Endpoint),
}

/// A session accepted by the host's user with `permissions`.
async fn session_with(permissions: Permissions) -> Running {
    let accept = EndpointConfig {
        accept: true,
        ..Default::default()
    };
    let host_endpoint = hext_net::bind(&DeviceKey::generate().unwrap(), &accept)
        .await
        .unwrap();
    let addr = host_endpoint.addr();
    let viewer_key = DeviceKey::generate().unwrap();
    let codes = SessionCodes::default();
    let code = codes.issue(
        viewer_key.public_key(),
        Grant {
            permissions,
            name: "Técnico".into(),
            id: "482 913 557".into(),
            needs_password: false,
            remember: false,
            bound: false,
            via: None,
            account: None,
        },
    );
    let desktop = Counting::default();
    let displays = desktop.displays.clone();
    let injected = desktop.injected.clone();
    let copied = desktop.copied.clone();
    let pasted = desktop.pasted.clone();
    let indicator = Arc::new(Indicator::default());
    let config = Arc::new(HostConfig {
        owners: None,
        peer: PeerInfo::local("test", "host"),
        access_code: None,
        session_codes: codes,
        desktop: Arc::new(desktop),
        presence: Some(indicator.clone()),
        unattended: None,
        resumes: Default::default(),
        restart: None,
        files: None,
    });
    let accepting = host_endpoint.clone();
    let host = tokio::spawn(async move {
        let conn = accepting.accept().await.unwrap().await.unwrap();
        host::serve(conn, config).await
    });
    let viewer_endpoint = hext_net::bind(&viewer_key, &EndpointConfig::default())
        .await
        .unwrap();
    let conn = hext_net::connect(&viewer_endpoint, addr).await.unwrap();
    let session = client::start(
        conn,
        Hello {
            peer: PeerInfo::local("test", "viewer"),
            access_code: code,
            resume: None,
            asks: Default::default(),
            audio: false,
        },
        None,
        DecoderPreference::Software,
        |_| {},
    )
    .await
    .unwrap();
    Running {
        session,
        displays,
        injected,
        copied,
        pasted,
        indicator,
        host,
        _endpoints: (host_endpoint, viewer_endpoint),
    }
}

/// Sends every kind of input a viewer could try; returns how much of it
/// reached the desktop.
async fn try_input(running: &Running) -> u32 {
    for event in [
        InputEvent::MouseMove { x: 0.5, y: 0.5 },
        InputEvent::Key {
            scancode: 0x1E,
            pressed: true,
        },
        InputEvent::SecureAttention,
    ] {
        running.session.send_input(event);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    running.injected.load(Ordering::SeqCst)
}

const WITH_CLIPBOARD: Permissions = Permissions {
    clipboard: true,
    ..Permissions::CONTROL
};

#[tokio::test(flavor = "multi_thread")]
async fn the_clipboard_goes_both_ways_with_the_permission() {
    let mut running = session_with(WITH_CLIPBOARD).await;
    let (outgoing, mut incoming) = running.session.clipboard().unwrap();

    outgoing.send("do técnico".into()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*running.pasted.lock().unwrap(), ["do técnico"]);

    let copied = running.copied.lock().unwrap().clone().unwrap();
    copied.send("do host".into()).await.unwrap();
    assert_eq!(incoming.recv().await.as_deref(), Some("do host"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_permission_the_clipboard_stays_put() {
    let mut running = session_with(Permissions::CONTROL).await;
    let (outgoing, _incoming) = running.session.clipboard().unwrap();

    // What a modified viewer would try.
    outgoing.send("do técnico".into()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(running.pasted.lock().unwrap().is_empty());
    assert!(
        running.copied.lock().unwrap().is_none(),
        "nothing is watched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn too_long_a_text_stays_out_of_the_host() {
    let mut running = session_with(WITH_CLIPBOARD).await;
    let (outgoing, _incoming) = running.session.clipboard().unwrap();

    outgoing.send("x".repeat(MAX_CLIPBOARD_TEXT + 1)).unwrap();
    outgoing.send("curto".into()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*running.pasted.lock().unwrap(), ["curto"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_viewer_learns_its_permissions() {
    let granted = Permissions {
        clipboard: true,
        ..Permissions::CONTROL
    };
    let running = session_with(granted).await;
    assert_eq!(running.session.permissions, granted);
    let shown = running.indicator.shown.lock().unwrap().clone().unwrap();
    assert_eq!(shown.name, "Técnico");
    assert_eq!(shown.id.as_deref(), Some("482 913 557"));
    assert_eq!(shown.permissions, granted);
}

#[tokio::test(flavor = "multi_thread")]
async fn control_needs_the_permission() {
    let allowed = session_with(Permissions::CONTROL).await;
    assert_eq!(try_input(&allowed).await, 3);

    // Even from a viewer that ignores what it was told: this one sends
    // input all the same.
    let refused = session_with(Permissions::VIEW_ONLY).await;
    assert_eq!(try_input(&refused).await, 0);
    assert!(!refused.session.is_finished(), "the session goes on");
}

/// The next thing the chat brings.
async fn chat_news(chat: &mut mpsc::Receiver<client::ChatNews>) -> client::ChatNews {
    tokio::time::timeout(Duration::from_secs(5), chat.recv())
        .await
        .expect("the chat answers")
        .expect("the session is on")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chat_goes_both_ways_even_if_only_seeing() {
    let mut running = session_with(Permissions::VIEW_ONLY).await;
    let mut chat = running.session.chat().unwrap();
    assert!(running.session.chat().is_none(), "once");

    let text = ChatText::new("Pode abrir o programa?").unwrap();
    running.session.send_chat(text.clone());
    let told = tokio::time::timeout(Duration::from_secs(5), running.indicator.chat_in.notified());
    told.await.expect("the user reads it");
    assert_eq!(running.indicator.read.lock().unwrap().clone(), [text]);

    let answer = ChatText::new("Já abri").unwrap();
    let out = running.indicator.chat_tx.clone();
    out.send(answer.clone()).unwrap();
    assert_eq!(chat_news(&mut chat).await, client::ChatNews::Text(answer));
    assert_eq!(try_input(&running).await, 0, "and the mouse stays out");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_with_no_one_there_is_answered_so() {
    let mut running = session_with(Permissions::CONTROL).await;
    let mut chat = running.session.chat().unwrap();
    running.indicator.nobody.store(true, Ordering::SeqCst);
    running
        .session
        .send_chat(ChatText::new("Tem alguém aí?").unwrap());
    assert_eq!(chat_news(&mut chat).await, client::ChatNews::NoOne);
    assert!(!running.session.is_finished(), "the session goes on");
}

/// Waits for the host to tell `displays`.
async fn told(running: &Running, displays: Displays) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while running.session.displays() != displays {
        assert!(tokio::time::Instant::now() < deadline, "the host told");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_viewer_learns_the_displays_and_chooses_one() {
    let running = session_with(Permissions::CONTROL).await;
    let first = Displays {
        count: 2,
        current: 0,
        ..Default::default()
    };
    told(&running, first).await;
    running.session.select_display(1);
    let second = Displays {
        count: 2,
        current: 1,
        ..Default::default()
    };
    told(&running, second).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn seeing_covers_every_display_but_the_mouse_stays_out() {
    let running = session_with(Permissions::VIEW_ONLY).await;
    running.session.select_display(1);
    let second = Displays {
        count: 2,
        current: 1,
        ..Default::default()
    };
    told(&running, second).await;
    assert_eq!(
        try_input(&running).await,
        0,
        "on this display as on the other"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_display_that_does_not_exist_changes_nothing() {
    let running = session_with(Permissions::CONTROL).await;
    running.session.select_display(u8::MAX);
    running.session.select_display(1);
    let second = Displays {
        count: 2,
        current: 1,
        ..Default::default()
    };
    told(&running, second).await;
    assert!(!running.session.is_finished(), "the session goes on");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_display_past_the_list_is_not_believed() {
    let running = session_with(Permissions::CONTROL).await;
    let first = Displays {
        count: 2,
        current: 0,
        ..Default::default()
    };
    told(&running, first).await;

    running.displays.send_replace(Displays {
        count: 2,
        current: 7,
        ..Default::default()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(running.session.displays(), first);

    let second = Displays {
        count: 2,
        current: 1,
        ..Default::default()
    };
    running.displays.send_replace(second);
    told(&running, second).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_ends_the_session() {
    let running = session_with(Permissions::CONTROL).await;
    assert_eq!(running.indicator.ended.load(Ordering::SeqCst), 0);
    let end = running.indicator.end.lock().unwrap().clone().unwrap();
    end.notify_one();

    tokio::time::timeout(Duration::from_secs(5), running.host)
        .await
        .expect("the session ends")
        .unwrap()
        .unwrap();
    assert_eq!(running.indicator.ended.load(Ordering::SeqCst), 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !running.session.is_finished() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the viewer was told"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
