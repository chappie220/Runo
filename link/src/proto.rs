//! The messages on the control channel (0), as JSON. A client sends
//! [`Call`]s; the device answers each with a [`Down::Reply`] carrying the
//! call's `id`, and sends [`Event`]s on its own.
//!
//! Channels other than 0 are numbered by the client when it opens them
//! (`Open`, `Attach`, `FileGet`, `FilePut`), so it is listening before the
//! first byte can arrive.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when a change would break the other end; both send theirs.
pub const VERSION: u32 = 1;
/// The device's TCP port.
pub const PORT: u16 = 7340;
/// The DNS-SD service type.
pub const SERVICE: &str = "_runo._tcp.local.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The Agent (`runo tui`).
    Agent,
    /// A login shell, as `!shell` gives.
    Shell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    // Pairing, the only calls an unknown client may make (4.4).
    /// SHA-256 of the client's nonce, sent before it sees the device's.
    PairBegin {
        commit: String,
    },
    /// The client's nonce, once it has the device's.
    PairReveal {
        nonce: String,
    },
    /// The device password, and what to call this client.
    PairFinish {
        password: String,
        name: String,
    },

    // Everything below needs a trusted client.
    Status {
        #[serde(default)]
        lang: Option<String>,
    },
    Logs {
        #[serde(default)]
        lang: Option<String>,
        #[serde(default)]
        lines: Option<usize>,
    },
    /// The device's terminal sessions.
    Sessions,
    /// A new session on `channel`.
    Open {
        channel: u32,
        kind: Kind,
        cols: u16,
        rows: u16,
    },
    /// An existing session on `channel`: what it showed lately, then live.
    Attach {
        channel: u32,
        session: u64,
        cols: u16,
        rows: u16,
    },
    Resize {
        channel: u32,
        cols: u16,
        rows: u16,
    },
    /// Stops showing a session here; it goes on.
    Detach {
        channel: u32,
    },
    /// Ends a session.
    Kill {
        session: u64,
    },
    /// A file's bytes on `channel`, then an empty frame.
    FileGet {
        channel: u32,
        path: String,
    },
    /// Bytes for `path` follow on `channel`, then an empty frame; the
    /// device answers with `FileDone`.
    FilePut {
        channel: u32,
        path: String,
        size: u64,
        #[serde(default)]
        mode: Option<u32>,
    },
    /// The trusted clients.
    Clients,
    /// Takes a client's trust away and drops its connections.
    Revoke {
        client: String,
    },
    /// Opens or closes the pairing window.
    Pairing {
        open: bool,
    },
    /// The device password, from a trusted client outside the local
    /// network; nothing else is answered there before it.
    Login {
        password: String,
    },
    /// The configuration changes waiting for confirmation.
    Unconfirmed,
    /// Keeps a change waiting for confirmation, or puts it back now.
    Confirm {
        change: u64,
        keep: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Down {
    Reply {
        re: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        ok: Value,
    },
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// First on every connection.
    Welcome {
        device: String,
        name: String,
        version: u32,
        /// Whether this client is trusted; if not it may only pair.
        trusted: bool,
        /// Whether the pairing window is open.
        pairing: bool,
        /// Whether this trusted client, reaching the device from outside
        /// the local network, must log in with the device password first.
        #[serde(default)]
        login: bool,
    },
    /// A session shown on `channel` ended.
    Exit {
        channel: u32,
        #[serde(default)]
        code: Option<i32>,
    },
    /// A `FilePut` on `channel` was written, or not.
    FileDone {
        channel: u32,
        #[serde(default)]
        error: Option<String>,
    },
    /// The device is closing this connection.
    Closed { reason: String },
}

// Replies.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairNonce {
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paired {
    /// This client's ID on the device.
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: u64,
    pub kind: Kind,
    /// Unix time.
    pub started: i64,
    pub running: bool,
    /// How many connections show it.
    pub attached: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    /// Unix time.
    pub added: i64,
    #[serde(default)]
    pub last_seen: Option<i64>,
    pub connected: bool,
    /// The client asking.
    #[serde(default)]
    pub this: bool,
}

/// A configuration change that may have cut the device off: it is put
/// back at `until` unless a client confirms it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unconfirmed {
    pub id: u64,
    pub summary: String,
    /// Unix time.
    pub until: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingState {
    pub open: bool,
    /// Unix time it closes, while open.
    #[serde(default)]
    pub until: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_have_a_plain_json_shape() {
        let call = Call {
            id: 3,
            request: Request::Open {
                channel: 1,
                kind: Kind::Agent,
                cols: 80,
                rows: 24,
            },
        };
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            serde_json::json!({"id": 3, "op": "open", "channel": 1, "kind": "agent", "cols": 80, "rows": 24})
        );
        let back: Call = serde_json::from_str(r#"{"id":4,"op":"sessions"}"#).unwrap();
        assert!(matches!(back.request, Request::Sessions));
        let status: Call = serde_json::from_str(r#"{"id":5,"op":"status"}"#).unwrap();
        assert!(matches!(status.request, Request::Status { lang: None }));

        let reply = Down::Reply {
            re: 3,
            error: None,
            ok: serde_json::json!({"text": "hi"}),
        };
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"reply":{"re":3,"ok":{"text":"hi"}}}"#
        );
        let event = Down::Event(Event::Exit {
            channel: 2,
            code: Some(0),
        });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":{"kind":"exit","channel":2,"code":0}}"#
        );
    }
}
