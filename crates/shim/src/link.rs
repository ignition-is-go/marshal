//! Plain-HTTP plumbing shared by the hook subcommands: reach the daemon on
//! whichever resolved address accepts, and read its reply, including why it
//! failed and which delivered messages to ack.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Connect to the first address of `target` that accepts. A name like
/// `localhost` can resolve to `::1` ahead of the `127.0.0.1` the daemon binds,
/// so stopping at the first address would be refused forever. Each attempt is
/// capped at [`CONNECT_ATTEMPT`], however long the caller's request may run.
pub fn connect_any(target: impl ToSocketAddrs, timeout: Duration) -> Option<TcpStream> {
    let attempt = timeout.min(CONNECT_ATTEMPT);
    target
        .to_socket_addrs()
        .ok()?
        .find_map(|addr| TcpStream::connect_timeout(&addr, attempt).ok())
}

const CONNECT_ATTEMPT: Duration = Duration::from_secs(5);

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

/// A complete HTTP response, or why there isn't a usable one.
pub fn parse_response(buf: &[u8]) -> Result<Reply, PostFailure> {
    let text = String::from_utf8_lossy(buf);
    let idx = text.find("\r\n\r\n").ok_or(PostFailure::Unreachable)?;
    let status = text[..idx]
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(PostFailure::Unreachable)?;
    if !(200..300).contains(&status) {
        return Err(PostFailure::Status(status));
    }
    let ack_ids = text[..idx]
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("x-marshal-ack"))
        .map(|(_, ids)| {
            ids.split(',')
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let restarting = text[..idx]
        .lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(name, _)| name.eq_ignore_ascii_case("x-marshal-restarting"));
    Ok(Reply {
        body: text[idx + 4..].to_string(),
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
            parse_response(b"HTTP/1.1 200 OK\r\nX-Marshal-Ack: a, b\r\n\r\nbody"),
            Ok(Reply {
                body: "body".to_string(),
                ack_ids: vec!["a".to_string(), "b".to_string()],
                restarting: false,
            })
        );
        let stopping =
            parse_response(b"HTTP/1.1 200 OK\r\nX-Marshal-Restarting: 1\r\n\r\n").unwrap();
        assert!(stopping.restarting && stopping.body.is_empty());
        for code in [401, 403] {
            let raw = format!("HTTP/1.1 {code} No\r\n\r\n");
            let failure = parse_response(raw.as_bytes()).unwrap_err();
            assert_eq!(failure, PostFailure::Status(code));
            assert!(failure.is_refusal());
        }
        let outage = parse_response(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").unwrap_err();
        assert!(!outage.is_refusal());
        assert_eq!(parse_response(b""), Err(PostFailure::Unreachable));
    }

    #[test]
    fn connect_any_skips_an_address_that_refuses() {
        let (_held, refused) = refusing_address();
        let live = TcpListener::bind("127.0.0.1:0").unwrap();
        let live_addr = live.local_addr().unwrap();

        let stream = connect_any(&[refused, live_addr][..], Duration::from_secs(1))
            .expect("should fall through to the address that accepts");
        assert_eq!(stream.peer_addr().unwrap(), live_addr);
    }

    #[test]
    fn connect_any_is_none_when_every_address_refuses() {
        let (_held, refused) = refusing_address();
        assert!(connect_any(&[refused][..], Duration::from_secs(1)).is_none());
    }
}
