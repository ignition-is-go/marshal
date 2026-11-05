//! Plain-HTTP plumbing shared by the hook subcommands: reach the daemon on
//! whichever resolved address accepts, and read its reply, including why it
//! failed and which delivered messages to ack.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Connect to the first address of `target` that accepts. A name like
/// `localhost` can resolve to `::1` ahead of the `127.0.0.1` the daemon binds,
/// so stopping at the first address would be refused forever. Each attempt is
/// capped at [`CONNECT_ATTEMPT`], however long the caller's request may run.
pub fn connect_any(target: impl ToSocketAddrs, deadline: Instant) -> Option<TcpStream> {
    target.to_socket_addrs().ok()?.find_map(|addr| {
        let attempt = deadline
            .checked_duration_since(Instant::now())?
            .min(CONNECT_ATTEMPT);
        if attempt.is_zero() {
            return None;
        }
        TcpStream::connect_timeout(&addr, attempt).ok()
    })
}

const CONNECT_ATTEMPT: Duration = Duration::from_secs(1);

/// Why a POST to the daemon got no usable answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostFailure {
    /// Nothing answered: the daemon is down, or this machine is asleep or off
    /// the network.
    Unreachable,
    /// The daemon answered with this non-2xx status.
    Status(u16),
}

impl PostFailure {
    /// Whether the daemon, not the network, said no, so retrying won't help
    /// until someone changes something.
    pub fn is_refusal(self) -> bool {
        matches!(self, Self::Status(401 | 403))
    }
}

impl std::fmt::Display for PostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable => write!(f, "can't reach the daemon (down, or no network)"),
            Self::Status(code @ (401 | 403)) => {
                write!(f, "the daemon refused this request ({code})")
            }
            Self::Status(code) => write!(f, "the daemon answered with an error ({code})"),
        }
    }
}

/// A successful response: its body, and the message ids the daemon wants acked
/// once they've been handed on.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub body: String,
    pub ack_ids: Vec<String>,
    /// The daemon ended this wait because it's stopping, so a retry will
    /// briefly find it unreachable.
    pub restarting: bool,
}

pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

fn response_length(buf: &[u8]) -> Result<Option<usize>, PostFailure> {
    let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buf[..idx]).map_err(|_| PostFailure::Unreachable)?;
    let mut length = None;
    for line in head.split("\r\n").skip(1) {
        let (name, value) = line.split_once(':').ok_or(PostFailure::Unreachable)?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(PostFailure::Unreachable);
        }
        if name.eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(PostFailure::Unreachable);
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| PostFailure::Unreachable)?,
            );
        }
    }
    let total = (idx + 4)
        .checked_add(length.ok_or(PostFailure::Unreachable)?)
        .filter(|n| *n <= MAX_RESPONSE_BYTES)
        .ok_or(PostFailure::Unreachable)?;
    Ok(Some(total))
}

pub fn read_reply(stream: &mut TcpStream, deadline: Instant) -> Result<Reply, PostFailure> {
    let mut buf = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        if let Some(total) = response_length(&buf)?
            && buf.len() >= total
        {
            return parse_response(&buf);
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(PostFailure::Unreachable)?;
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| PostFailure::Unreachable)?;
        let cap = chunk
            .len()
            .min(MAX_RESPONSE_BYTES.saturating_sub(buf.len()) + 1);
        let read = stream
            .read(&mut chunk[..cap])
            .map_err(|_| PostFailure::Unreachable)?;
        if read == 0 {
            return Err(PostFailure::Unreachable);
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(PostFailure::Unreachable);
        }
    }
}

/// Parse only a complete, unambiguous response before exposing acknowledgment IDs.
pub fn parse_response(buf: &[u8]) -> Result<Reply, PostFailure> {
    if response_length(buf)? != Some(buf.len()) {
        return Err(PostFailure::Unreachable);
    }
    let text = std::str::from_utf8(buf).map_err(|_| PostFailure::Unreachable)?;
    let idx = text.find("\r\n\r\n").ok_or(PostFailure::Unreachable)?;
    let mut lines = text[..idx].split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(PostFailure::Unreachable)?;
    if !(200..300).contains(&status) {
        return Err(PostFailure::Status(status));
    }
    let mut ack_ids = None;
    let mut restarting = false;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(PostFailure::Unreachable)?;
        if value.bytes().any(|b| b < 0x20 && b != b'\t' || b == 0x7f) {
            return Err(PostFailure::Unreachable);
        }
        if name.eq_ignore_ascii_case("x-marshal-ack-v1") {
            if ack_ids.is_some() {
                return Err(PostFailure::Unreachable);
            }
            ack_ids = Some(
                serde_json::from_str::<Vec<String>>(value.trim())
                    .map_err(|_| PostFailure::Unreachable)?,
            );
        }
        if name.eq_ignore_ascii_case("x-marshal-restarting") {
            restarting = true;
        }
    }
    let ack_ids = ack_ids.unwrap_or_default();
    let body = text[idx + 4..].to_string();
    if body.trim().is_empty() && !ack_ids.is_empty() {
        return Err(PostFailure::Unreachable);
    }
    Ok(Reply {
        body,
        ack_ids,
        restarting,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener};

    /// A loopback address that refuses connections, held for as long as the
    /// returned socket lives. It's bound but not listening, so the port can't be
    /// handed to another test running in parallel, as a dropped listener's can.
    fn refusing_address() -> (socket2::Socket, SocketAddr) {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket
            .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        let addr = socket.local_addr().unwrap().as_socket().unwrap();
        (socket, addr)
    }

    #[test]
    fn a_response_says_why_it_failed() {
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nX-Marshal-Ack-V1: [\"a\",\"b\"]\r\n\r\nbody"),
            Ok(Reply {
                body: "body".to_string(),
                ack_ids: vec!["a".to_string(), "b".to_string()],
                restarting: false,
            })
        );
        let stopping = parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Marshal-Restarting: 1\r\n\r\n",
        )
        .unwrap();
        assert!(stopping.restarting && stopping.body.is_empty());
        for code in [401, 403] {
            let raw = format!("HTTP/1.1 {code} No\r\nContent-Length: 0\r\n\r\n");
            let failure = parse_response(raw.as_bytes()).unwrap_err();
            assert_eq!(failure, PostFailure::Status(code));
            assert!(failure.is_refusal());
        }
        let outage =
            parse_response(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n").unwrap_err();
        assert!(!outage.is_refusal());
        assert_eq!(parse_response(b""), Err(PostFailure::Unreachable));
    }

    fn frame(headers: &str, body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    #[test]
    fn malformed_or_incomplete_frames_cannot_expose_acknowledgments() {
        for raw in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nX-Marshal-Ack-V1: [\"m\"]\r\n\r\npartial"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\n\r\nbody".to_vec(),
            frame("Content-Length: 4\r\n", b"body"),
            frame("Transfer-Encoding: chunked\r\n", b"body"),
            frame(
                "X-Marshal-Ack-V1: [\"m\"]\r\nX-Marshal-Ack-V1: []\r\n",
                b"body",
            ),
            frame("X-Marshal-Ack-V1: nope\r\n", b"body"),
            frame("X-Marshal-Ack-V1: [1]\r\n", b"body"),
            frame("X-Marshal-Ack-V1: [\"m\"]\r\n", b""),
            frame("", &[0xff]),
        ] {
            assert!(parse_response(&raw).is_err(), "accepted {raw:?}");
        }
        let mut extra = frame("", b"body");
        extra.push(b'x');
        assert!(parse_response(&extra).is_err());
    }

    #[test]
    fn acknowledgment_ids_round_trip_without_identity_normalization() {
        let ids = vec![
            "",
            "comma,id",
            "cr\r\nX-Injected: yes",
            "quote\"slash\\",
            "\u{7f}",
            "café",
        ];
        let encoded = serde_json::to_string(&ids)
            .unwrap()
            .replace('\u{7f}', "\\u007f");
        let raw = frame(&format!("X-Marshal-Ack-V1: {encoded}\r\n"), "é".as_bytes());
        let reply = parse_response(&raw).unwrap();
        assert_eq!(reply.ack_ids, ids);
        assert_eq!(reply.body, "é");
        assert!(
            parse_response(&frame("X-Marshal-Ack: [\"old-id\"]\r\n", b"body"))
                .unwrap()
                .ack_ids
                .is_empty()
        );
    }

    #[test]
    fn read_reply_finishes_at_the_frame_boundary_without_waiting_for_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            use std::io::Write;
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(&frame("", b"body")).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let reply = read_reply(&mut stream, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(reply.body, "body");
        release_tx.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn connect_any_skips_an_address_that_refuses() {
        let (_held, refused) = refusing_address();
        let live = TcpListener::bind("127.0.0.1:0").unwrap();
        let live_addr = live.local_addr().unwrap();

        let stream = connect_any(
            &[refused, live_addr][..],
            Instant::now() + Duration::from_secs(1),
        )
        .expect("should fall through to the address that accepts");
        assert_eq!(stream.peer_addr().unwrap(), live_addr);
    }

    #[test]
    fn connect_any_is_none_when_every_address_refuses() {
        let (_held, refused) = refusing_address();
        assert!(connect_any(&[refused][..], Instant::now() + Duration::from_secs(1)).is_none());
    }
}
