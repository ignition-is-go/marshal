//! marshal's own minimal plain-HTTP listener for the Claude Code
//! `/hook/*` endpoints.
//!
//! Deliberately separate from myko's listener. Stock myko serves
//! `/myko/mcp` (WS + HTTP MCP) on its own port and we do **not** fork it
//! to bolt on extra routes. The hooks need a dumb plain-HTTP endpoint a
//! curl one-liner POSTs raw hook JSON to, so marshal binds a second port
//! and shares the server's `Arc<CellServerCtx>` — same registry, same
//! event log, no second source of truth.
//!
//! The HTTP handling is intentionally tiny: HTTP/1.1, `Content-Length`
//! bodies only (curl `--data-binary @-` always sends one), one request
//! per connection, `Connection: close`. Hook bodies are a few hundred
//! bytes; anything fancier would be a dependency we don't need.

use std::{net::SocketAddr, sync::Arc};

use myko::server::CellServerCtx;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::hooks;

/// Hard cap on a hook request body. Claude Code hook payloads are small;
/// this only exists so a malformed/hostile `Content-Length` can't make us
/// allocate unboundedly.
const MAX_BODY: usize = 256 * 1024;

/// Run the hook listener forever. Spawn on a tokio task.
pub async fn run(bind: SocketAddr, ctx: Arc<CellServerCtx>) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    log::info!("marshal hook listener on http://{bind} (/hook/*)");
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("[hook] accept error: {e}");
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, ctx).await {
                log::debug!("[hook] connection error: {e}");
            }
        });
    }
}

async fn handle_conn(mut stream: TcpStream, ctx: Arc<CellServerCtx>) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];

    // Read until the end of headers.
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            // Connection closed before a complete header block.
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_BODY + 8192 {
            return write_response(&mut stream, 413, "payload too large").await;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut rl = request_line.split_whitespace();
    let method = rl.next().unwrap_or("");
    let target = rl.next().unwrap_or("");

    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_BODY);

    // Body bytes already buffered past the header terminator, plus any
    // remainder up to Content-Length.
    let body_start = header_end + 4;
    let mut body: Vec<u8> = buf.get(body_start..).map(|s| s.to_vec()).unwrap_or_default();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if method != "POST" {
        return write_response(&mut stream, 405, "method not allowed").await;
    }

    match hooks::dispatch(path, query, &body, &ctx) {
        Some(text) => write_response(&mut stream, 200, &text).await,
        None => write_response(&mut stream, 404, "not found").await,
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// First index of `needle` in `haystack`, or `None`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}
