//! Files in a real session, in process: the hash, the permission, and what
//! a modified viewer could try.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hext_codec::DecoderPreference;
use hext_crypto::DeviceKey;
use hext_net::{Connection, EndpointConfig};
use hext_proto::{
    FileMessage, Hello, InputEvent, MAX_FILE_CHUNK, PeerInfo, Permissions, StreamKind, VideoFrame,
};
use hext_session::files::{Confirm, Direction, OwnFiles, Stage, Tell, Transfer};
use hext_session::host::{ActiveSession, BoxFuture, Desktop, HostConfig, Presence, Screen};
use hext_session::{Grant, SessionCodes, client, host};
use ring::digest;
use tokio::sync::{Notify, mpsc};

const WITH_FILES: Permissions = Permissions {
    files: true,
    ..Permissions::CONTROL
};

struct StillDesktop;

impl Desktop for StillDesktop {
    fn open(
        &self,
        frames: mpsc::Sender<VideoFrame>,
        _clipboard: Option<mpsc::Sender<String>>,
    ) -> BoxFuture<'_, hext_session::Result<Box<dyn Screen>>> {
        Box::pin(async move { Ok(Box::new(StillScreen(frames)) as Box<dyn Screen>) })
    }

    fn bitrate_bps(&self) -> u32 {
        1_000_000
    }
}

/// Open while the session runs: a closed one ends it.
struct StillScreen(#[allow(dead_code)] mpsc::Sender<VideoFrame>);

impl Screen for StillScreen {
    fn request_keyframe(&self) {}
    fn inject(&mut self, _event: &InputEvent) {}
    fn set_clipboard(&mut self, _text: String) {}
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// A bar with a button for choosing files, keeping what it was told.
#[derive(Default)]
struct Bar {
    chosen: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>>,
    told: Mutex<Vec<Transfer>>,
}

impl Presence for Bar {
    fn started(&self, _session: &ActiveSession) -> Arc<Notify> {
        Arc::default()
    }

    fn ended(&self) {}

    fn outgoing(&self) -> Option<mpsc::UnboundedReceiver<PathBuf>> {
        self.chosen.lock().unwrap().take()
    }

    fn transfer(&self, transfer: &Transfer) {
        self.told.lock().unwrap().push(transfer.clone());
    }
}

/// A folder of a test's own, gone with it.
struct Folder(PathBuf);

impl Folder {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let number = NEXT.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("hext-files-{}-{name}-{number}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn holds(&self, name: &str, content: &[u8]) -> bool {
        std::fs::read(self.0.join(name)).is_ok_and(|found| found == content)
    }

    /// Waits for `names` to be all it has.
    async fn settles_on(&self, names: &[&str]) {
        for _ in 0..500 {
            if self.names() == names {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(self.names(), names);
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Running {
    session: client::ClientHandle,
    conn: Connection,
    bar: Arc<Bar>,
    choose: mpsc::UnboundedSender<PathBuf>,
    host_downloads: Folder,
    viewer_downloads: Folder,
    _endpoints: (hext_net::Endpoint, hext_net::Endpoint),
}

async fn session_with(permissions: Permissions) -> (Running, mpsc::UnboundedReceiver<Transfer>) {
    session_answering(permissions, true).await
}

/// A session whose technician says `yes` to each file from the host.
async fn session_answering(
    permissions: Permissions,
    yes: bool,
) -> (Running, mpsc::UnboundedReceiver<Transfer>) {
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
    let (choose, chosen) = mpsc::unbounded_channel();
    let bar = Arc::new(Bar {
        chosen: Mutex::new(Some(chosen)),
        told: Mutex::default(),
    });
    let host_downloads = Folder::new("host");
    let viewer_downloads = Folder::new("viewer");
    let config = Arc::new(HostConfig {
        owners: None,
        peer: PeerInfo::local("test", "host"),
        access_code: None,
        session_codes: codes,
        desktop: Arc::new(StillDesktop),
        presence: Some(bar.clone()),
        unattended: None,
        resumes: Default::default(),
        restart: None,
        files: Some(Arc::new(OwnFiles(Some(host_downloads.0.clone())))),
    });
    let accepting = host_endpoint.clone();
    tokio::spawn(async move {
        let conn = accepting.accept().await.unwrap().await.unwrap();
        host::serve(conn, config).await
    });
    let viewer_endpoint = hext_net::bind(&viewer_key, &EndpointConfig::default())
        .await
        .unwrap();
    let conn = hext_net::connect(&viewer_endpoint, addr).await.unwrap();
    let mut session = client::start(
        conn.clone(),
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
    let (told, news) = mpsc::unbounded_channel();
    let tell: Tell = Arc::new(move |transfer| {
        let _ = told.send(transfer);
    });
    let confirm: Confirm = Arc::new(move |_, _| Box::pin(async move { yes }));
    tokio::spawn(session.files(
        Arc::new(OwnFiles(Some(viewer_downloads.0.clone()))),
        tell,
        confirm,
    ));
    let running = Running {
        session,
        conn,
        bar,
        choose,
        host_downloads,
        viewer_downloads,
        _endpoints: (host_endpoint, viewer_endpoint),
    };
    (running, news)
}

/// The last piece of news, once a transfer is over.
async fn outcome(news: &mut mpsc::UnboundedReceiver<Transfer>) -> Transfer {
    loop {
        let transfer = tokio::time::timeout(Duration::from_secs(10), news.recv())
            .await
            .expect("news in time")
            .expect("news");
        if !matches!(transfer.stage, Stage::Going { .. }) {
            return transfer;
        }
    }
}

fn content(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .try_into()
        .unwrap()
}

fn source(folder: &Folder, name: &str, bytes: &[u8]) -> PathBuf {
    let path = folder.0.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// What a modified viewer can put on a stream of its own.
async fn raw_file(conn: &Connection, messages: &[FileMessage]) {
    let mut stream = hext_net::open_uni(conn, StreamKind::File).await.unwrap();
    for message in messages {
        // (Refused midway, the rest has nowhere to go.)
        let _ = stream.send(message).await;
    }
    stream.finish();
}

async fn nothing_lands(place: &Path) {
    tokio::time::sleep(Duration::from_millis(400)).await;
    let left = std::fs::read_dir(place).unwrap().count();
    assert_eq!(left, 0, "something stayed in {}", place.display());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_reaches_the_hosts_downloads_whole() {
    let (running, mut news) = session_with(WITH_FILES).await;
    let origin = Folder::new("origin");
    // More than a chunk, and not a round number of them.
    let bytes = content(3 * MAX_FILE_CHUNK + 17);
    running
        .session
        .send_file(source(&origin, "relatório.pdf", &bytes));

    let done = outcome(&mut news).await;
    assert_eq!(done.stage, Stage::Done, "{done}");
    assert_eq!(done.direction, Direction::Sending);
    assert_eq!(running.host_downloads.names(), ["relatório.pdf"]);
    assert!(running.host_downloads.holds("relatório.pdf", &bytes));
    let told = running.bar.told.lock().unwrap();
    let last = told.last().expect("the bar heard");
    assert_eq!(
        (last.direction, &last.stage),
        (Direction::Receiving, &Stage::Done)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_reaches_the_technician_from_the_bar() {
    let (running, mut news) = session_with(WITH_FILES).await;
    let origin = Folder::new("origin");
    let bytes = content(MAX_FILE_CHUNK + 1);
    running
        .choose
        .send(source(&origin, "tela.png", &bytes))
        .unwrap();

    let done = outcome(&mut news).await;
    assert_eq!(done.stage, Stage::Done, "{done}");
    assert_eq!(done.direction, Direction::Receiving);
    assert!(running.viewer_downloads.holds("tela.png", &bytes));
    assert_eq!(running.viewer_downloads.names(), ["tela.png"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_file_is_a_file() {
    let (running, mut news) = session_with(WITH_FILES).await;
    let origin = Folder::new("origin");
    running.session.send_file(source(&origin, "vazio.txt", b""));
    assert_eq!(outcome(&mut news).await.stage, Stage::Done);
    assert!(running.host_downloads.holds("vazio.txt", b""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_of_the_same_name_stays_as_it_was() {
    let (running, mut news) = session_with(WITH_FILES).await;
    source(&running.host_downloads, "a.txt", b"do cliente");
    let origin = Folder::new("origin");
    running.session.send_file(source(&origin, "a.txt", b"novo"));
    assert_eq!(outcome(&mut news).await.stage, Stage::Done);
    assert!(running.host_downloads.holds("a.txt", b"do cliente"));
    assert!(running.host_downloads.holds("a (1).txt", b"novo"));
}

#[tokio::test(flavor = "multi_thread")]
async fn files_dropped_together_all_arrive() {
    let (running, mut news) = session_with(WITH_FILES).await;
    let origin = Folder::new("origin");
    for name in ["1.txt", "2.txt", "3.txt"] {
        running
            .session
            .send_file(source(&origin, name, name.as_bytes()));
    }
    for _ in 0..3 {
        assert_eq!(outcome(&mut news).await.stage, Stage::Done);
    }
    assert_eq!(running.host_downloads.names(), ["1.txt", "2.txt", "3.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_does_not_go() {
    let (running, mut news) = session_with(WITH_FILES).await;
    let origin = Folder::new("origin");
    let folder = origin.0.join("pasta");
    std::fs::create_dir(&folder).unwrap();
    running.session.send_file(folder);
    assert_eq!(
        outcome(&mut news).await.stage,
        Stage::Failed("pastas não podem ser enviadas".into())
    );
    nothing_lands(&running.host_downloads.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_technician_can_say_no_to_a_file_from_the_host() {
    let (running, mut news) = session_answering(WITH_FILES, false).await;
    let origin = Folder::new("origin");
    running
        .choose
        .send(source(&origin, "suspeito.exe", b"MZ"))
        .unwrap();
    assert_eq!(
        outcome(&mut news).await.stage,
        Stage::Failed("recusado".into())
    );
    nothing_lands(&running.viewer_downloads.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_permission_nothing_lands_even_from_a_modified_viewer() {
    let (running, _news) = session_with(Permissions::CONTROL).await;
    let bytes = b"sem permissao".to_vec();
    raw_file(
        &running.conn,
        &[
            FileMessage::Start {
                id: 1,
                name: "invasor.exe".into(),
                size: bytes.len() as u64,
            },
            FileMessage::Chunk(bytes.clone()),
            FileMessage::End {
                sha256: sha256(&bytes),
            },
        ],
    )
    .await;
    nothing_lands(&running.host_downloads.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_permission_the_bar_sends_nothing() {
    let (running, mut news) = session_with(Permissions::CONTROL).await;
    let origin = Folder::new("origin");
    let _ = running.choose.send(source(&origin, "a.txt", b"x"));
    running.session.send_file(origin.0.join("a.txt"));
    assert!(matches!(outcome(&mut news).await.stage, Stage::Failed(_)));
    nothing_lands(&running.viewer_downloads.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_that_is_a_path_is_refused() {
    let (running, _news) = session_with(WITH_FILES).await;
    for name in [
        "../fora.txt",
        "..\\fora.txt",
        "C:\\fora.txt",
        "a.txt:oculto",
    ] {
        raw_file(
            &running.conn,
            &[
                FileMessage::Start {
                    id: 1,
                    name: name.into(),
                    size: 1,
                },
                FileMessage::Chunk(vec![1]),
                FileMessage::End {
                    sha256: sha256(&[1]),
                },
            ],
        )
        .await;
    }
    nothing_lands(&running.host_downloads.0).await;
    let parent = running.host_downloads.0.parent().unwrap();
    assert!(!parent.join("fora.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn what_does_not_match_its_hash_is_thrown_away() {
    let (running, _news) = session_with(WITH_FILES).await;
    raw_file(
        &running.conn,
        &[
            FileMessage::Start {
                id: 1,
                name: "a.txt".into(),
                size: 3,
            },
            FileMessage::Chunk(vec![1, 2, 3]),
            FileMessage::End { sha256: [0; 32] },
        ],
    )
    .await;
    nothing_lands(&running.host_downloads.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn more_or_less_than_announced_is_thrown_away() {
    let (running, _news) = session_with(WITH_FILES).await;
    for (announced, bytes) in [(2u64, vec![1u8, 2, 3]), (4, vec![1, 2, 3])] {
        raw_file(
            &running.conn,
            &[
                FileMessage::Start {
                    id: 1,
                    name: "a.txt".into(),
                    size: announced,
                },
                FileMessage::Chunk(bytes.clone()),
                FileMessage::End {
                    sha256: sha256(&bytes),
                },
            ],
        )
        .await;
    }
    nothing_lands(&running.host_downloads.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_cut_off_leaves_nothing_behind() {
    let (running, _news) = session_with(WITH_FILES).await;
    let folder = running.host_downloads.0.clone();
    let mut stream = hext_net::open_uni(&running.conn, StreamKind::File)
        .await
        .unwrap();
    let start = FileMessage::Start {
        id: 1,
        name: "grande.bin".into(),
        size: 1 << 30,
    };
    stream.send(&start).await.unwrap();
    stream
        .send(&FileMessage::Chunk(vec![1; 1024]))
        .await
        .unwrap();
    // The temporary file is there, with its whole size...
    for _ in 0..500 {
        if std::fs::read_dir(&folder).unwrap().count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    // ...until the session is gone.
    running.session.close().await.unwrap();
    running.host_downloads.settles_on(&[]).await;
    drop(running.viewer_downloads);
}
