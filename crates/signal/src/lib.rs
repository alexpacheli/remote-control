//! Signaling between the apps and `hext-server`.
//!
//! Every app keeps one WebSocket open to the server. The server opens with a
//! random challenge; the app answers by signing it with its device key
//! ([`auth_message`]), which proves it owns that key, and the server replies
//! with the device's 9-digit [`DeviceId`] (assigned on first contact, the
//! same for that key ever after). From then on the server relays connection
//! requests between devices that are online. Messages are JSON text frames,
//! tagged by `"type"`.

mod client;
mod id;

use hext_crypto::{PublicKey, Signature};
use serde::{Deserialize, Serialize};

pub use client::{Client, ClientError};
pub use id::{DeviceId, ParseDeviceIdError};

/// Signaling protocol version: both sides must speak the same one.
pub const PROTOCOL: u16 = 1;

/// Path of the WebSocket endpoint on the server.
pub const SIGNAL_PATH: &str = "/v1/signal";

/// Bytes in the server's challenge.
pub const NONCE_LEN: usize = 32;

/// What a device signs to log in: a fixed context string and the server's
/// challenge (so the signature is useless anywhere else).
pub fn auth_message(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut message = b"hext-signal-auth-v1\0".to_vec();
    message.extend_from_slice(nonce);
    message
}

/// Longest device name the server keeps.
pub const MAX_NAME_LEN: usize = 64;

/// Server to app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// First message: sign it to log in.
    Challenge {
        protocol: u16,
        #[serde(with = "hex_array")]
        nonce: [u8; NONCE_LEN],
    },
    /// Logged in.
    Welcome { id: DeviceId },
    /// (To a host) a device wants to connect. Answer with
    /// [`ClientMsg::Accept`] or [`ClientMsg::Reject`] for `request`.
    Request {
        request: u64,
        from: DeviceId,
        #[serde(with = "hex_key")]
        from_key: PublicKey,
        from_name: String,
    },
    /// (To a viewer) the host accepted: connect to it, and expect its key.
    Accepted {
        request: u64,
        #[serde(with = "hex_key")]
        host_key: PublicKey,
        /// How to reach the host (transport-specific, e.g. `ip:port`).
        addrs: Vec<String>,
    },
    /// (To a viewer) the request went nowhere.
    Rejected { request: u64, reason: Rejection },
    /// (To a host) a request it was asked about is over (the viewer left
    /// or it timed out): stop asking the user.
    Cancelled { request: u64 },
    /// Something went wrong; the server closes the connection.
    Error { message: String },
}

/// App to server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// The answer to [`ServerMsg::Challenge`].
    Hello {
        protocol: u16,
        #[serde(with = "hex_key")]
        public_key: PublicKey,
        #[serde(with = "hex_sig")]
        signature: Signature,
        device_name: String,
        app_version: String,
    },
    /// Ask to connect to device `to`. `request` is chosen by the viewer and
    /// comes back in the answer.
    Connect { request: u64, to: DeviceId },
    /// (Host) accept a [`ServerMsg::Request`].
    Accept { request: u64, addrs: Vec<String> },
    /// (Host) refuse a [`ServerMsg::Request`].
    Reject { request: u64 },
}

/// Why a connection request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// The host's user refused.
    Declined,
    /// No device with that ID is online (or it does not exist: the server
    /// does not tell them apart, so IDs cannot be probed).
    Offline,
    /// The host did not answer in time.
    Timeout,
    /// Too many requests from this device lately.
    TooManyRequests,
    /// The host has too many requests waiting.
    Busy,
    /// A device cannot connect to itself.
    SelfConnect,
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("messages serialize")
    }
}

impl ClientMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("messages serialize")
    }
}

mod hex_array {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer, const N: usize>(v: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        let text = <&str>::deserialize(d)?;
        let mut out = [0; N];
        hex::decode_to_slice(text, &mut out).map_err(D::Error::custom)?;
        Ok(out)
    }
}

mod hex_key {
    use hext_crypto::PublicKey;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(v: &PublicKey, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PublicKey, D::Error> {
        <&str>::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}

mod hex_sig {
    use hext_crypto::Signature;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Signature, s: S) -> Result<S::Ok, S::Error> {
        super::hex_array::serialize(&v.to_bytes(), s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Signature, D::Error> {
        super::hex_array::deserialize(d).map(Signature::from_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hext_crypto::DeviceKey;

    #[test]
    fn messages_roundtrip_as_tagged_json() {
        let key = DeviceKey::generate().unwrap();
        let id: DeviceId = "482 913 557".parse().unwrap();
        let msgs = [
            ClientMsg::Hello {
                protocol: PROTOCOL,
                public_key: key.public_key(),
                signature: key.sign(b"x"),
                device_name: "PC".into(),
                app_version: "0.1.0".into(),
            },
            ClientMsg::Connect { request: 7, to: id },
            ClientMsg::Accept {
                request: 9,
                addrs: vec!["192.168.0.10:7420".into()],
            },
            ClientMsg::Reject { request: 9 },
        ];
        for msg in msgs {
            let json = msg.to_json();
            assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), msg);
        }
        let json = ServerMsg::Rejected {
            request: 7,
            reason: Rejection::TooManyRequests,
        }
        .to_json();
        assert_eq!(
            json,
            r#"{"type":"rejected","request":7,"reason":"too_many_requests"}"#
        );
        let challenge = ServerMsg::Challenge {
            protocol: PROTOCOL,
            nonce: [0xab; NONCE_LEN],
        };
        assert_eq!(
            serde_json::from_str::<ServerMsg>(&challenge.to_json()).unwrap(),
            challenge
        );
    }

    #[test]
    fn bad_fields_are_rejected() {
        assert!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"connect","request":1,"to":12}"#).is_err()
        );
        assert!(
            serde_json::from_str::<ServerMsg>(r#"{"type":"challenge","protocol":1,"nonce":"zz"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn the_signed_message_is_bound_to_the_challenge() {
        let key = DeviceKey::generate().unwrap();
        let sig = key.sign(&auth_message(&[1; NONCE_LEN]));
        assert!(
            key.public_key()
                .verify(&auth_message(&[1; NONCE_LEN]), &sig)
        );
        assert!(
            !key.public_key()
                .verify(&auth_message(&[2; NONCE_LEN]), &sig)
        );
        assert!(!key.public_key().verify(&[1; NONCE_LEN], &sig));
    }
}
