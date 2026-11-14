//! Hext Remote wire protocol.
//!
//! A session uses one QUIC connection with two kinds of traffic:
//! - a reliable, bidirectional **control stream** that starts with a
//!   [preamble](encode_preamble) from each side and then carries [`Control`]
//!   messages as length-prefixed frames (see [`frame`]);
//! - **unidirectional streams**, each starting with a [`StreamKind`] byte:
//!   input events (client → host) and files (either way, one stream each);
//! - unreliable **QUIC datagrams** carrying [`Datagram`] messages: video
//!   ([`VideoPacket`]s: each [`VideoFrame`] split into shards with
//!   Reed-Solomon parity, so a lost packet neither stalls later frames nor
//!   needs a retransmission), sound ([`AudioPacket`]s: 10 ms of Opus each,
//!   only while something plays) and ping.
//!
//! All messages are serialized with `postcard`.

use serde::{Deserialize, Serialize};

pub mod frame;

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u16 = 13;

/// The longest text the clipboard shares, in bytes: it fits a control frame.
pub const MAX_CLIPBOARD_TEXT: usize = 512 * 1024;

/// The longest message of the chat, in bytes.
pub const MAX_CHAT_TEXT: usize = 8 * 1024;

/// The most of a file carried by one [`FileMessage::Chunk`].
pub const MAX_FILE_CHUNK: usize = 64 * 1024;

/// TLS ALPN identifier. It carries no version: versions are compared through
/// the preamble, so mismatched peers still connect and can report why they fail.
pub const ALPN: &[u8] = b"hext";

/// Size of the control stream preamble.
pub const PREAMBLE_LEN: usize = 4;

const PREAMBLE_MAGIC: [u8; 2] = *b"HX";

/// Each side opens the control stream with `b"HX"` followed by its protocol
/// version (`u16` little-endian). This layout never changes, so peers of any
/// version can detect a mismatch before decoding versioned messages.
pub fn encode_preamble(version: u16) -> [u8; PREAMBLE_LEN] {
    let v = version.to_le_bytes();
    [PREAMBLE_MAGIC[0], PREAMBLE_MAGIC[1], v[0], v[1]]
}

/// Returns the protocol version announced in a preamble.
pub fn decode_preamble(bytes: [u8; PREAMBLE_LEN]) -> Result<u16, CodecError> {
    if bytes[..2] != PREAMBLE_MAGIC {
        return Err(CodecError::BadPreamble);
    }
    Ok(u16::from_le_bytes([bytes[2], bytes[3]]))
}

/// Why a connection closes when the host's user ended the session: nobody
/// should come back to it, whatever was under way.
pub const ENDED_ON_HOST: &[u8] = b"ended on host";

/// Default UDP port a host listens on for direct (LAN) connections.
pub const DEFAULT_PORT: u16 = 7420;

/// Upper bound for one encoded video frame on the video stream.
pub const MAX_VIDEO_FRAME_LEN: usize = 16 * 1024 * 1024;

/// First byte of every unidirectional stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    // 1 was the video stream (protocol 3 and earlier); video now travels in
    // datagrams.
    /// Client → host, carries [`InputEvent`]s.
    Input = 2,
    /// Either way, carries one file as [`FileMessage`]s.
    File = 3,
}

impl StreamKind {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            2 => Some(Self::Input),
            3 => Some(Self::File),
            _ => None,
        }
    }

    /// Largest frame accepted on a stream of this kind.
    pub const fn max_frame_len(self) -> usize {
        match self {
            Self::Input | Self::File => frame::MAX_FRAME_LEN,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// Identifies a peer during the handshake.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub app_version: String,
    pub device_name: String,
    pub os: Os,
}

impl PeerInfo {
    pub fn local(app_version: impl Into<String>, device_name: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
            device_name: device_name.into(),
            os: Os::current(),
        }
    }
}

/// What the host's user lets the viewer do besides seeing the screen. It
/// holds on the host: what was not granted is refused there, not only hidden
/// in the viewer.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Permissions {
    /// Mouse and keyboard (and Ctrl+Alt+Del).
    pub control: bool,
    pub clipboard: bool,
    pub files: bool,
    pub restart: bool,
}

impl Permissions {
    pub const VIEW_ONLY: Self = Self {
        control: false,
        clipboard: false,
        files: false,
        restart: false,
    };
    /// What a yes without a choice of permissions means.
    pub const CONTROL: Self = Self {
        control: true,
        ..Self::VIEW_ONLY
    };
}

/// The client's introduction.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub peer: PeerInfo,
    /// Code shown by the host; it must match before anything is shared.
    pub access_code: String,
    /// Coming back to a session: the token it gave ([`Control::ResumeToken`]).
    pub resume: Option<[u8; 32]>,
    /// What the viewer itself asks of the host, end to end: the server's
    /// word alone is not enough. Last on purpose: a viewer from before it
    /// sends nothing here and gets the default (asks nothing), and a host
    /// from before ignores the bytes after the fields it knows.
    #[serde(default, deserialize_with = "absent_is_default")]
    pub asks: HelloAsks,
    /// The viewer plays the host's sound: send it from the start. After
    /// `asks`, last for the same reason: a viewer from before gets no sound
    /// (it would not know [`Datagram::Audio`]).
    #[serde(default, deserialize_with = "absent_is_default")]
    pub audio: bool,
}

/// What a viewer that got in with the unattended password asks the host for.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HelloAsks {
    /// To be trusted from now on, without the password.
    pub remember: bool,
    /// That the host bind to this account (the chain's ID).
    pub account: Option<[u8; 32]>,
}

/// The value, or its default when the peer sent nothing (an older version:
/// postcard has no field names, so a missing last field is the end of input).
fn absent_is_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(T::deserialize(deserializer).unwrap_or_default())
}

/// Messages on the reliable control stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// First message after the preamble, sent by the client.
    Hello(Hello),
    /// Host's answer to an acceptable [`Control::Hello`].
    HelloAck {
        peer: PeerInfo,
        permissions: Permissions,
    },
    /// Host refuses the session; the connection is closed afterwards.
    Reject(RejectReason),
    /// Client asks for a fresh keyframe (start of stream, decode error).
    RequestKeyframe,
    /// Client lacks these packets (block, index) of a video frame, or all of
    /// them if none is named.
    ResendVideo {
        frame: u64,
        packets: Vec<(u16, u16)>,
    },
    /// Orderly end of session.
    Bye,
    /// Host's answer to a [`Control::Hello`] that takes the unattended
    /// password: the salt it was stretched with, and the host's message of
    /// the password exchange.
    PasswordChallenge {
        salt: [u8; 16],
        #[serde(with = "serde_bytes")]
        message: Vec<u8>,
    },
    /// The client's message of the exchange, and its proof of the result.
    PasswordProof {
        #[serde(with = "serde_bytes")]
        message: Vec<u8>,
        #[serde(with = "serde_bytes")]
        proof: Vec<u8>,
    },
    /// The password was right: the host's proof, before [`Control::HelloAck`].
    PasswordOk {
        #[serde(with = "serde_bytes")]
        confirmation: Vec<u8>,
    },
    /// Host → client: how to come back to this session, once.
    ResumeToken([u8; 32]),
    /// Client → host: restart the computer (takes [`Permissions::restart`]).
    Restart,
    /// Host → client: whether the computer is restarting.
    RestartAnswer { ok: bool },
    /// Either way: the clipboard changed to this text (takes
    /// [`Permissions::clipboard`], at most [`MAX_CLIPBOARD_TEXT`] bytes).
    Clipboard { text: String },
    /// What became of file `id` ([`FileMessage::Start`]), from the side that
    /// got it.
    FileAnswer { id: u64, outcome: FileOutcome },
    /// Host → client, at the start and at each change.
    Displays(Displays),
    /// Client → host: show this display. Seeing the screen takes no
    /// permission of its own, and that covers every display.
    SelectDisplay { index: u8 },
    /// A message of the chat, either way.
    Chat { text: ChatText },
    /// Host → client: nobody at that computer can read the chat.
    ChatNoOne,
    /// Client → host: turn the virtual monitor on (and show it) or off. Only
    /// to a host that offers one ([`Displays::virtual_display`]): an older
    /// host does not know this message.
    VirtualDisplay { on: bool },
    /// Client → host: the virtual monitor's next size, round through the
    /// ones the host offers. Only when it says it is `resizable`
    /// ([`VirtualDisplay::On`]).
    NextVirtualSize,
    /// Client → host: the privacy screen on or off (takes
    /// [`Permissions::control`]). Only to a host that offers it
    /// ([`Displays::privacy`]): an older host does not know this message.
    Privacy { on: bool },
    /// Client → host: send the computer's sound, or stop (muted in the
    /// viewer). It takes no permission: the sound is part of seeing the
    /// screen. Only to a host that offers it ([`Displays::audio`]).
    Audio { on: bool },
}

/// A message of the chat. It comes from the other computer, so what cannot be
/// shown as plain text (control characters, marks that reverse how the text
/// reads) goes as it arrives, and it cannot reach a log through `Debug`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(from = "String", into = "String")]
pub struct ChatText(String);

impl ChatText {
    /// `None`: nothing to send.
    pub fn new(text: &str) -> Option<Self> {
        Some(Self::from(text.to_owned())).filter(|text| !text.is_empty())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for ChatText {
    fn from(text: String) -> Self {
        let clean = |c: &char| {
            let reordering = matches!(
                c,
                '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            );
            let breaking = matches!(c, '\u{2028}' | '\u{2029}');
            *c == '\n' || !(c.is_control() || reordering || breaking)
        };
        let mut clean: String = text.chars().filter(clean).collect();
        if clean.len() > MAX_CHAT_TEXT {
            let end = (0..=MAX_CHAT_TEXT)
                .rev()
                .find(|end| clean.is_char_boundary(*end))
                .unwrap_or(0);
            clean.truncate(end);
        }
        Self(clean.trim().to_owned())
    }
}

impl From<ChatText> for String {
    fn from(text: ChatText) -> Self {
        text.0
    }
}

impl std::fmt::Debug for ChatText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChatText({} bytes)", self.0.len())
    }
}

/// The displays a host has, numbered from 0, and the one being shown.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Displays {
    pub count: u8,
    pub current: u8,
    /// Whether the host can show a monitor of its own for the session. Last
    /// on purpose: an older host sends nothing ([`VirtualDisplay::None`]),
    /// and an older viewer ignores it.
    #[serde(default, deserialize_with = "absent_is_default")]
    pub virtual_display: VirtualDisplay,
    /// Whether the host can hide its screen from the person there while the
    /// technician works. After `virtual_display`, last for the same reason.
    #[serde(default, deserialize_with = "absent_is_default")]
    pub privacy: Privacy,
    /// Whether the host can send its sound ([`Control::Audio`]). After
    /// `privacy`, last for the same reason.
    #[serde(default, deserialize_with = "absent_is_default")]
    pub audio: bool,
}

/// The privacy screen of a host (docs/planos/F7.md): the person at the
/// computer sees a black screen with a notice, and their mouse and keyboard
/// are blocked; the technician sees and controls everything.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Privacy {
    /// The host has none (an older one, another system, an older Windows).
    #[default]
    None,
    /// It can turn it on ([`Control::Privacy`]).
    Off,
    /// It is on.
    On,
}

/// The virtual monitor of a host (docs/monitor-virtual.md): a monitor that
/// exists only for the session, for a computer with none to show.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VirtualDisplay {
    /// The host has none (or is older).
    #[default]
    None,
    /// It can turn one on ([`Control::VirtualDisplay`]).
    Off,
    /// It is on, as display `index`, at `width`×`height`. `resizable`: the
    /// Hext turned it on, and [`Control::NextVirtualSize`] changes its size.
    On {
        index: u8,
        width: u16,
        height: u16,
        resizable: bool,
    },
}

/// What the receiving side did with a file.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
    Saved,
    /// No permission, or a name that is not just a name.
    Refused,
    /// Nobody is logged on to that computer to have a Downloads folder.
    NoUser,
    /// It could not be written, or did not arrive whole.
    Failed,
}

/// What a [`StreamKind::File`] stream carries: [`Self::Start`], the file in
/// [`Self::Chunk`]s, and [`Self::End`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FileMessage {
    Start { id: u64, name: String, size: u64 },
    Chunk(#[serde(with = "serde_bytes")] Vec<u8>),
    End { sha256: [u8; 32] },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Wrong access code.
    WrongCode,
    Denied,
    Busy,
    WrongPassword,
    /// Too many wrong passwords: try again in this many seconds.
    LockedOut {
        seconds: u32,
    },
}

/// Messages sent as unreliable QUIC datagrams.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Datagram {
    /// Host → client: one shard of a video frame.
    Video(VideoPacket),
    /// Host → client, while nothing changes on screen: the number of the
    /// last frame sent, so a frame that lost every packet is noticed.
    VideoIdle {
        last_frame: u64,
    },
    /// RTT probe; the receiver echoes it back as [`Datagram::Pong`].
    Ping {
        seq: u32,
        sent_micros: u64,
    },
    Pong {
        seq: u32,
        sent_micros: u64,
    },
    /// Host → client: 10 ms of the computer's sound. Only to a viewer that
    /// asked for it ([`Hello::audio`]): an older one does not know it.
    Audio(AudioPacket),
}

/// 10 ms of sound: Opus, 48 kHz, stereo. Numbered one by one as they are
/// sent; nothing is sent (and no number used) while the computer is silent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u32,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// The packet before this one again (`seq - 1`), so one lost packet is
    /// rebuilt from the next; empty after a silence.
    #[serde(with = "serde_bytes")]
    pub prev: Vec<u8>,
}

/// One shard of a serialized [`VideoFrame`].
///
/// The frame's bytes are cut into `shard`-byte data shards (the last one
/// zero-padded), grouped into blocks of `block_size`; each block also gets
/// `parity` Reed-Solomon parity shards, so any `k` of its `k + parity`
/// packets rebuild its `k` data shards. Data shard `i` of block `b` holds
/// the frame bytes from `(b * block_size + i) * shard`; `index` counts data
/// shards first, then parity.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    /// Frame number ([`VideoFrame::seq`]).
    pub frame: u64,
    /// Size of the serialized frame, in bytes.
    pub len: u32,
    /// Bytes per shard (the same for every packet of the frame).
    pub shard: u16,
    /// Data shards per block (the last block may have fewer).
    pub block_size: u16,
    pub block: u16,
    pub index: u16,
    /// Parity shards of this block.
    pub parity: u16,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// One encoded H.264 access unit.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub seq: u64,
    /// The display this shows ([`Displays::current`]).
    pub display: u8,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    /// Time spent capturing (copying) this frame on the host.
    pub capture_micros: u32,
    /// Time spent encoding this frame on the host.
    pub encode_micros: u32,
    /// Encoded by the GPU (shown in the viewer's stats).
    pub hardware: bool,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Input from the viewer, injected on the host.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute position, normalized to `0.0..=1.0` over the shared display.
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll in notches (lines). Positive `dy` scrolls up, positive `dx` right.
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// PC set-1 scancode; `0xE0xx` marks extended keys.
    Key {
        scancode: u16,
        pressed: bool,
    },
    /// Release every key and button still held (viewer lost focus).
    ReleaseAll,
    /// Ctrl+Alt+Del: the secure attention sequence, which only the system
    /// (the Windows service) can send; a plain key press would not do.
    SecureAttention,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid message: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("not a Hext Remote control stream (bad preamble)")]
    BadPreamble,
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge { len: usize, max: usize },
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_stdvec(msg)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let msgs = [
            Control::Hello(Hello {
                peer: PeerInfo::local("0.1.0", "desk"),
                access_code: "123456789".into(),
                resume: Some([7; 32]),
                asks: HelloAsks {
                    remember: true,
                    account: Some([3; 32]),
                },
                audio: true,
            }),
            Control::RequestKeyframe,
            Control::ResendVideo {
                frame: 7,
                packets: vec![(0, 3), (1, 70)],
            },
            Control::ResumeToken([9; 32]),
            Control::Restart,
            Control::RestartAnswer { ok: true },
            Control::Clipboard {
                text: "copiado".into(),
            },
            Control::FileAnswer {
                id: 3,
                outcome: FileOutcome::NoUser,
            },
            Control::Displays(Displays {
                count: 3,
                current: 1,
                virtual_display: VirtualDisplay::On {
                    index: 2,
                    width: 1920,
                    height: 1080,
                    resizable: true,
                },
                privacy: Privacy::On,
                audio: true,
            }),
            Control::SelectDisplay { index: 2 },
            Control::VirtualDisplay { on: true },
            Control::NextVirtualSize,
            Control::Privacy { on: true },
            Control::Audio { on: false },
            Control::Chat {
                text: ChatText::new("Pode reiniciar?").unwrap(),
            },
            Control::ChatNoOne,
            Control::HelloAck {
                peer: PeerInfo::local("0.1.0", "laptop"),
                permissions: Permissions {
                    files: true,
                    ..Permissions::CONTROL
                },
            },
            Control::Reject(RejectReason::Busy),
            Control::Reject(RejectReason::LockedOut { seconds: 300 }),
            Control::Bye,
            Control::PasswordChallenge {
                salt: [7; 16],
                message: vec![1; 33],
            },
            Control::PasswordProof {
                message: vec![2; 33],
                proof: vec![3; 32],
            },
            Control::PasswordOk {
                confirmation: vec![4; 32],
            },
        ];
        for msg in msgs {
            let bytes = encode(&msg).unwrap();
            assert_eq!(decode::<Control>(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn displays_go_both_ways_between_versions() {
        #[derive(Serialize, Deserialize)]
        struct OldDisplays {
            count: u8,
            current: u8,
        }
        // From an older host: no virtual monitor.
        let old = encode(&OldDisplays {
            count: 2,
            current: 1,
        })
        .unwrap();
        let displays: Displays = decode(&old).unwrap();
        assert_eq!(displays.virtual_display, VirtualDisplay::None);
        assert_eq!(displays.privacy, Privacy::None);
        assert_eq!((displays.count, displays.current), (2, 1));
        // From a host with the virtual monitor and no privacy screen (0.8).
        #[derive(Serialize, Deserialize)]
        struct DisplaysOf08 {
            count: u8,
            current: u8,
            virtual_display: VirtualDisplay,
        }
        let of_08 = encode(&DisplaysOf08 {
            count: 1,
            current: 0,
            virtual_display: VirtualDisplay::Off,
        })
        .unwrap();
        let displays: Displays = decode(&of_08).unwrap();
        assert_eq!(displays.virtual_display, VirtualDisplay::Off);
        assert_eq!(displays.privacy, Privacy::None);
        assert!(!displays.audio);
        // From a host with the privacy screen and no sound.
        #[derive(Serialize, Deserialize)]
        struct DisplaysWithPrivacy {
            count: u8,
            current: u8,
            virtual_display: VirtualDisplay,
            privacy: Privacy,
        }
        let with_privacy = encode(&DisplaysWithPrivacy {
            count: 1,
            current: 0,
            virtual_display: VirtualDisplay::Off,
            privacy: Privacy::Off,
        })
        .unwrap();
        let displays: Displays = decode(&with_privacy).unwrap();
        assert_eq!(displays.privacy, Privacy::Off);
        assert!(!displays.audio);
        // To an older viewer: what it knows, the rest ignored.
        let new = encode(&Displays {
            count: 3,
            current: 2,
            virtual_display: VirtualDisplay::On {
                index: 2,
                width: 1280,
                height: 720,
                resizable: false,
            },
            privacy: Privacy::On,
            audio: true,
        })
        .unwrap();
        let seen: OldDisplays = decode(&new).unwrap();
        assert_eq!((seen.count, seen.current), (3, 2));
        let seen: DisplaysOf08 = decode(&new).unwrap();
        assert_eq!(seen.count, 3);
        let seen: DisplaysWithPrivacy = decode(&new).unwrap();
        assert_eq!(seen.privacy, Privacy::On);
    }

    #[test]
    fn file_roundtrip() {
        let msgs = [
            FileMessage::Start {
                id: 3,
                name: "relatório.pdf".into(),
                size: 1 << 40,
            },
            FileMessage::Chunk(vec![7; MAX_FILE_CHUNK]),
            FileMessage::End { sha256: [9; 32] },
        ];
        for msg in msgs {
            let bytes = encode(&msg).unwrap();
            assert!(bytes.len() <= frame::MAX_FRAME_LEN);
            assert_eq!(decode::<FileMessage>(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn datagram_roundtrip() {
        let msg = Datagram::Ping {
            seq: 42,
            sent_micros: 1_234_567,
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode::<Datagram>(&bytes).unwrap(), msg);

        let packet = Datagram::Video(VideoPacket {
            frame: 1 << 40,
            len: 300_000,
            shard: 1100,
            block_size: 64,
            block: 4,
            index: 70,
            parity: 13,
            data: vec![7; 1100],
        });
        let bytes = encode(&packet).unwrap();
        // The header stays small next to the shard.
        assert!(bytes.len() <= 1100 + 32, "{} bytes", bytes.len());
        assert_eq!(decode::<Datagram>(&bytes).unwrap(), packet);

        let sound = Datagram::Audio(AudioPacket {
            seq: u32::MAX,
            data: vec![1; 160],
            prev: vec![2; 150],
        });
        let bytes = encode(&sound).unwrap();
        assert!(bytes.len() <= 160 + 150 + 16, "{} bytes", bytes.len());
        assert_eq!(decode::<Datagram>(&bytes).unwrap(), sound);
        // Video stays as it was: an older viewer reads its datagrams.
        assert_eq!(encode(&packet).unwrap()[0], 0);
    }

    #[test]
    fn a_chat_message_is_plain_text() {
        let text = ChatText::new("  oi\r\n\u{1b}[31m\u{202e}tudo bem?\u{7}  ").unwrap();
        assert_eq!(text.as_str(), "oi\n[31mtudo bem?");
        assert!(ChatText::new(" \n\u{202e}\t ").is_none());
    }

    #[test]
    fn a_chat_message_too_long_is_cut_between_characters() {
        let text = ChatText::new(&"é".repeat(MAX_CHAT_TEXT)).unwrap();
        assert_eq!(text.as_str().len(), MAX_CHAT_TEXT);
    }

    #[test]
    fn what_comes_through_the_network_is_cleaned_too() {
        let bytes = postcard::to_stdvec(&"\u{202e}oi\u{1b}".to_owned()).unwrap();
        let text: ChatText = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(text.as_str(), "oi");
    }

    #[test]
    fn a_chat_message_stays_out_of_the_logs() {
        let msg = Control::Chat {
            text: ChatText::new("a senha é 1234").unwrap(),
        };
        assert!(!format!("{msg:?}").contains("1234"));
    }

    #[test]
    fn video_and_input_roundtrip() {
        let frame = VideoFrame {
            seq: 7,
            display: 2,
            width: 1920,
            height: 1080,
            keyframe: true,
            capture_micros: 1200,
            encode_micros: 4800,
            hardware: true,
            data: vec![0, 0, 0, 1, 0x67, 0xff],
        };
        assert_eq!(
            decode::<VideoFrame>(&encode(&frame).unwrap()).unwrap(),
            frame
        );

        let input = InputEvent::Key {
            scancode: 0xE04B,
            pressed: true,
        };
        assert_eq!(
            decode::<InputEvent>(&encode(&input).unwrap()).unwrap(),
            input
        );
    }

    #[test]
    fn stream_kinds() {
        assert_eq!(
            StreamKind::from_byte(StreamKind::Input as u8),
            Some(StreamKind::Input)
        );
        assert_eq!(
            StreamKind::from_byte(StreamKind::File as u8),
            Some(StreamKind::File)
        );
        assert_eq!(StreamKind::from_byte(0), None);
        // The old video stream is refused.
        assert_eq!(StreamKind::from_byte(1), None);
    }

    /// A Hello from a version before [`HelloAsks`]: the same fields, minus
    /// the last.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct OldHello {
        peer: PeerInfo,
        access_code: String,
        resume: Option<[u8; 32]>,
    }

    #[test]
    fn hellos_travel_between_versions() {
        let old = OldHello {
            peer: PeerInfo::local("0.6.1", "desk"),
            access_code: "123456789".into(),
            resume: None,
        };
        // An older viewer asks nothing.
        let hello: Hello = decode(&encode(&old).unwrap()).unwrap();
        assert_eq!(hello.asks, HelloAsks::default());
        assert_eq!(hello.access_code, old.access_code);
        // An older host reads what it knows.
        let new = Hello {
            peer: old.peer.clone(),
            access_code: old.access_code.clone(),
            resume: Some([1; 32]),
            asks: HelloAsks {
                remember: true,
                account: Some([2; 32]),
            },
            audio: true,
        };
        let read: OldHello = decode(&encode(&new).unwrap()).unwrap();
        assert_eq!(read.resume, Some([1; 32]));
        // A viewer of 0.8 (asks, no sound) gets no sound; a host of 0.8
        // reads the asks.
        #[derive(Serialize, Deserialize)]
        struct HelloOf08 {
            peer: PeerInfo,
            access_code: String,
            resume: Option<[u8; 32]>,
            asks: HelloAsks,
        }
        let of_08 = HelloOf08 {
            peer: old.peer.clone(),
            access_code: old.access_code.clone(),
            resume: None,
            asks: HelloAsks {
                remember: true,
                account: None,
            },
        };
        let hello: Hello = decode(&encode(&of_08).unwrap()).unwrap();
        assert!(hello.asks.remember);
        assert!(!hello.audio);
        let read: HelloOf08 = decode(&encode(&new).unwrap()).unwrap();
        assert_eq!(read.asks, new.asks);
        // Inside a control frame, too.
        let mut buf =
            frame::encode_frame(&Control::Hello(new.clone()), frame::MAX_FRAME_LEN).unwrap();
        let back: Control = frame::take_frame(&mut buf, frame::MAX_FRAME_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(back, Control::Hello(new));
    }

    #[test]
    fn preamble_roundtrip() {
        assert_eq!(decode_preamble(encode_preamble(513)).unwrap(), 513);
        assert!(matches!(
            decode_preamble(*b"GET "),
            Err(CodecError::BadPreamble)
        ));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode::<Control>(&[0xff, 0xff, 0xff]).is_err());
    }
}
