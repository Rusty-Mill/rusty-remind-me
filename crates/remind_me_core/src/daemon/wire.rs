//! What a client and the daemon say to each other.
//!
//! Every connection opens with one [`Hello`] line from the client and one
//! [`Reply`] line back. After a welcome, the connection carries whichever
//! [`Mode`] the hello asked for:
//! - [`Mode::Mcp`]: MCP JSON-RPC, one request line in and exactly one line
//!   out (the response, or `null` for a notification), so the client always
//!   knows how much to read.
//! - [`Mode::Op`]: [`super::ops::Op`] lines, each answered by one
//!   [`super::ops::OpReply`] line.
//! - [`Mode::Http`]: one HTTP exchange with the dashboard API, bytes as a
//!   browser sent them.
//! - [`Mode::Control`]: only `status` and `shutdown`. Checked for the token
//!   alone, so `rusty-remind-me daemon stop` still reaches a daemon from
//!   another build or started under other settings.

use super::session::Session;
use super::settings::Fingerprint;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read, Write};

/// Bumped whenever a message changes shape.
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Mcp,
    Op,
    Http,
    Control,
}

/// A client's opening line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub token: String,
    /// See [`build_id`].
    pub build: String,
    pub mode: Mode,
    pub session: Session,
    pub settings: Fingerprint,
}

/// Why the daemon turned a client away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    Token,
    Protocol { daemon: u32 },
    Build { daemon: String },
    Settings { differing: Vec<String> },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Token => write!(f, "the daemon token did not match"),
            Refusal::Protocol { daemon } => {
                write!(
                    f,
                    "the daemon speaks protocol {daemon}, this client {PROTOCOL}"
                )
            }
            Refusal::Build { .. } => write!(
                f,
                "the daemon is a different build of rusty-remind-me; \
                 `rusty-remind-me daemon stop` lets the next client start this one"
            ),
            Refusal::Settings { differing } => write!(
                f,
                "this client's settings differ from the daemon's: {}",
                differing.join(", ")
            ),
        }
    }
}

/// The daemon's answer to a [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Welcome,
    Refused(Refusal),
}

/// Identifies the executable, not only its version.
///
/// A daemon outlives the binary that started it: after a rebuild or an
/// upgrade, the running daemon is still the old code, possibly expecting an
/// older schema. The version string alone would not notice a rebuild, so this
/// adds the executable's size and modification time.
pub fn build_id() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok());
    let stamp = exe
        .map(|meta| {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            format!("{}:{}", meta.len(), modified)
        })
        .unwrap_or_default();
    sha256::digest(format!("{}:{}", env!("CARGO_PKG_VERSION"), stamp))
}

/// Write `message` as one JSON line and flush.
pub fn write_line<T: Serialize + ?Sized>(out: &mut impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()
}

/// The longest [`Hello`] line the daemon will buffer.
///
/// A hello arrives before the token is checked, so any local process can send
/// one; without a cap, a peer that never sends a newline grows the buffer
/// without bound.
pub const MAX_HELLO_LINE: u64 = 64 * 1024;

/// Read one JSON line, or `None` at end of stream.
pub fn read_line<T: DeserializeOwned>(input: &mut impl BufRead) -> io::Result<Option<T>> {
    read_line_max(input, u64::MAX)
}

/// Like [`read_line`], but fails with `InvalidData` once the line passes
/// `max` bytes, without buffering the rest.
pub fn read_line_max<T: DeserializeOwned>(
    input: &mut impl BufRead,
    max: u64,
) -> io::Result<Option<T>> {
    let mut line = String::new();
    if (&mut *input)
        .take(max.saturating_add(1))
        .read_line(&mut line)?
        == 0
    {
        return Ok(None);
    }
    if line.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line longer than {max} bytes"),
        ));
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_round_trip() {
        for reply in [
            Reply::Welcome,
            Reply::Refused(Refusal::Token),
            Reply::Refused(Refusal::Settings {
                differing: vec!["REMIND_ME_SYNC_SECRET".into()],
            }),
        ] {
            let mut buf = Vec::new();
            write_line(&mut buf, &reply).unwrap();
            assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
            let back: Reply = read_line(&mut buf.as_slice()).unwrap().unwrap();
            assert_eq!(back, reply);
        }
    }

    #[test]
    fn end_of_stream_is_none_and_garbage_is_an_error() {
        assert!(read_line::<Reply>(&mut &b""[..]).unwrap().is_none());
        let err = read_line::<Reply>(&mut &b"nope\n"[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_line_past_the_cap_is_refused_without_reading_on() {
        let mut input = std::io::Cursor::new(vec![b'a'; 1000]);
        let err = read_line_max::<Reply>(&mut input, 100).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Only the cap plus one byte was consumed.
        assert_eq!(input.position(), 101);
    }

    #[test]
    fn a_line_within_the_cap_still_parses() {
        let mut buf = Vec::new();
        write_line(&mut buf, &Reply::Welcome).unwrap();
        let back: Reply = read_line_max(&mut buf.as_slice(), buf.len() as u64)
            .unwrap()
            .unwrap();
        assert_eq!(back, Reply::Welcome);
    }

    #[test]
    fn the_build_id_is_stable_within_a_process() {
        assert_eq!(build_id(), build_id());
    }
}
