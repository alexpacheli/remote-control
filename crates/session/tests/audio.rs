//! The host's sound reaches a viewer that asked for it, stops when muted,
//! and an older viewer (which asks nothing) gets none.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hext_codec::DecoderPreference;
use hext_crypto::DeviceKey;
use hext_net::EndpointConfig;
use hext_proto::{AudioPacket, Displays, Hello, InputEvent, PeerInfo, VideoFrame};
use hext_session::host::{BoxFuture, Desktop, HostConfig, Screen};
use hext_session::{client, host};
use tokio::sync::{mpsc, watch};

/// A desktop with sound: the test plays it, and hears what the session asks.
struct Sounding {
    play: Mutex<Option<mpsc::Receiver<AudioPacket>>>,
    /// Each `set_audio` the session made.
    asked: Arc<Mutex<Vec<bool>>>,
    displays: watch::Sender<Displays>,
}

impl Desktop for Sounding {
    fn open(
        &self,
        frames: mpsc::Sender<VideoFrame>,
        _clipboard: Option<mpsc::Sender<String>>,
    ) -> BoxFuture<'_, hext_session::Result<Box<dyn Screen>>> {
        Box::pin(async move {
            Ok(Box::new(SoundingScreen {
                _frames: frames,
                audio: self.play.lock().unwrap().take(),
                asked: self.asked.clone(),
                displays: self.displays.clone(),
            }) as Box<dyn Screen>)
        })
    }

    fn bitrate_bps(&self) -> u32 {
        1_000_000
    }
}

struct SoundingScreen {
    _frames: mpsc::Sender<VideoFrame>,
    audio: Option<mpsc::Receiver<AudioPacket>>,
    asked: Arc<Mutex<Vec<bool>>>,
    displays: watch::Sender<Displays>,
}

impl Screen for SoundingScreen {
    fn request_keyframe(&self) {}
    fn inject(&mut self, _event: &InputEvent) {}
    fn set_clipboard(&mut self, _text: String) {}

    fn displays(&self) -> Option<watch::Receiver<Displays>> {
        Some(self.displays.subscribe())
    }

    fn audio(&mut self) -> Option<mpsc::Receiver<AudioPacket>> {
        self.audio.take()
    }

    fn set_audio(&self, on: bool) {
        self.asked.lock().unwrap().push(on);
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct Running {
    session: client::ClientHandle,
    play: mpsc::Sender<AudioPacket>,
    asked: Arc<Mutex<Vec<bool>>>,
    _endpoints: (hext_net::Endpoint, hext_net::Endpoint),
}

async fn session(audio: bool) -> Running {
    let accept = EndpointConfig {
        accept: true,
        ..Default::default()
    };
    let host_endpoint = hext_net::bind(&DeviceKey::generate().unwrap(), &accept)
        .await
        .unwrap();
    let addr = host_endpoint.addr();
    let (play, played) = mpsc::channel(64);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let desktop = Sounding {
        play: Mutex::new(Some(played)),
        asked: asked.clone(),
        displays: watch::Sender::new(Displays {
            count: 1,
            current: 0,
            ..Default::default()
        }),
    };
    let config = Arc::new(HostConfig {
        owners: None,
        peer: PeerInfo::local("test", "host"),
        access_code: Some("123456789".into()),
        session_codes: Default::default(),
        desktop: Arc::new(desktop),
        presence: None,
        unattended: None,
        resumes: Default::default(),
        restart: None,
        files: None,
    });
    let accepting = host_endpoint.clone();
    tokio::spawn(async move {
        let conn = accepting.accept().await.unwrap().await.unwrap();
        host::serve(conn, config).await
    });
    let viewer_endpoint = hext_net::bind(&DeviceKey::generate().unwrap(), &Default::default())
        .await
        .unwrap();
    let conn = hext_net::connect(&viewer_endpoint, addr).await.unwrap();
    let session = client::start(
        conn,
        Hello {
            peer: PeerInfo::local("test", "viewer"),
            access_code: "123456789".into(),
            resume: None,
            asks: Default::default(),
            audio,
        },
        None,
        DecoderPreference::Software,
        |_| {},
    )
    .await
    .unwrap();
    Running {
        session,
        play,
        asked,
        _endpoints: (host_endpoint, viewer_endpoint),
    }
}

fn packet(seq: u32) -> AudioPacket {
    AudioPacket {
        seq,
        data: vec![seq as u8; 80],
        prev: Vec::new(),
    }
}

/// Waits until the host told its displays (with or without sound).
async fn told(session: &client::ClientHandle) -> Displays {
    let mut changed = session.displays_changed();
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.displays().count == 0 {
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("displays");
    session.displays()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sound_reaches_the_viewer_that_asked() {
    let mut running = session(true).await;
    let shown = told(&running.session).await;
    assert!(shown.audio, "the host says it has sound");
    assert_eq!(*running.asked.lock().unwrap(), [true], "on from the start");
    let mut heard = running.session.audio().unwrap();
    for seq in 0..5 {
        running.play.send(packet(seq)).await.unwrap();
    }
    for seq in 0..5 {
        let got = tokio::time::timeout(Duration::from_secs(5), heard.recv())
            .await
            .expect("sound")
            .unwrap();
        assert_eq!(got, packet(seq));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn muting_tells_the_host_to_stop() {
    let running = session(true).await;
    told(&running.session).await;
    running.session.set_audio(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*running.asked.lock().unwrap(), [true, false]);
    // The same word twice is told once.
    running.session.set_audio(false);
    running.session.set_audio(true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*running.asked.lock().unwrap(), [true, false, true]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_that_asks_nothing_gets_no_sound() {
    // A viewer of 0.8 sends no `audio` (false): the host keeps it off, and
    // sends no datagram it would not know.
    let mut running = session(false).await;
    assert!(!told(&running.session).await.audio);
    assert!(running.asked.lock().unwrap().is_empty());
    let _ = running.play.try_send(packet(0));
    let mut heard = running.session.audio().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), heard.recv())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn muted_before_the_host_tells_is_told_once_it_does() {
    // Coming back muted: the Hello asked for sound, the window is muted.
    let running = session(true).await;
    running.session.set_audio(false);
    told(&running.session).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*running.asked.lock().unwrap(), [true, false]);
}
