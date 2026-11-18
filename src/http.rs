//! One plain HTTP/1.1 exchange with a WhatsUp over a connected stream: the
//! command line talking to this machine's WhatsUp, and a forwarded start or
//! stop reaching a remote one through the SSH SOCKS tunnel.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Why an exchange has no reply to give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostError {
    /// Nothing accepted the connection: the tunnel is down, or nothing
    /// listens on the port.
    Connect,
    /// Connected, but no reply came back within the read timeout.
    Timeout,
    /// Connected, but the reply failed for another reason: the connection
    /// closed or reset, the request could not be sent, or the reply is not
    /// HTTP.
    Reply(String),
}

/// Sends one request to the WhatsUp at `127.0.0.1:<host_port>` over
/// `stream` and returns the status and body of its reply. A `body` is sent
/// as JSON. There is no `Origin` header, so the request passes WhatsUp's
/// guard, and the connection is used once (`Connection: close`).
pub fn exchange(
    mut stream: TcpStream,
    method: &str,
    host_port: u16,
    path: &str,
    body: Option<&str>,
    read_timeout: Duration,
) -> Result<(u16, String), PostError> {
    stream
        .set_read_timeout(Some(read_timeout))
        .and_then(|()| stream.set_write_timeout(Some(read_timeout)))
        .map_err(|err| PostError::Reply(err.to_string()))?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{host_port}\r\n");
    if let Some(body) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("Connection: close\r\n\r\n");
    request.push_str(body.unwrap_or_default());
    stream
        .write_all(request.as_bytes())
        .map_err(|err| PostError::Reply(format!("could not send the request: {err}")))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| match err.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => PostError::Timeout,
            _ => PostError::Reply(format!("no reply: {err}")),
        })?;
    if response.is_empty() {
        return Err(PostError::Reply(
            "the connection closed without a reply".into(),
        ));
    }
    let response = String::from_utf8_lossy(&response);
    let not_http = || PostError::Reply("the reply is not http".into());
    let (head, body) = response.split_once("\r\n\r\n").ok_or_else(not_http)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(not_http)?;
    Ok((status, body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::mpsc;
    use std::thread;

    const QUICK: Duration = Duration::from_secs(2);

    /// Reads one request head (a GET has no body), answers it with `reply`
    /// as raw bytes, and hands back what it read.
    fn one_reply(reply: &'static str) -> (u16, mpsc::Receiver<String>) {
        held_reply(reply, Duration::ZERO)
    }

    /// As [`one_reply`], but holds the connection silent for `hold` first.
    fn held_reply(reply: &'static str, hold: Duration) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(QUICK)).unwrap();
            let mut seen = Vec::new();
            let mut chunk = [0u8; 1024];
            while !seen.ends_with(b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => seen.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&seen).to_string());
            thread::sleep(hold);
            let _ = stream.write_all(reply.as_bytes());
        });
        (port, rx)
    }

    fn connect(port: u16) -> TcpStream {
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap()
    }

    #[test]
    fn get_sends_a_bare_request_and_reads_the_reply() {
        let (port, seen) = one_reply("HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\n{\"ok\":true}");
        let reply = exchange(connect(port), "GET", port, "/api/dashboard", None, QUICK);
        assert_eq!(reply, Ok((200, r#"{"ok":true}"#.to_string())));
        let request = seen.recv().unwrap();
        assert_eq!(
            request,
            format!(
                "GET /api/dashboard HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
        );
    }

    #[test]
    fn a_reply_that_is_not_http_is_an_error() {
        let (port, _seen) = one_reply("SSH-2.0-OpenSSH_9.6\r\n");
        let reply = exchange(connect(port), "GET", port, "/api/dashboard", None, QUICK);
        assert_eq!(reply, Err(PostError::Reply("the reply is not http".into())));
    }

    #[test]
    fn a_closed_connection_is_not_a_timeout() {
        let (port, _seen) = one_reply("");
        let reply = exchange(connect(port), "GET", port, "/api/dashboard", None, QUICK);
        assert_eq!(
            reply,
            Err(PostError::Reply(
                "the connection closed without a reply".into()
            ))
        );
    }

    #[test]
    fn a_silent_server_times_out() {
        let (port, _seen) = held_reply("", Duration::from_secs(1));
        let quick = Duration::from_millis(200);
        let reply = exchange(connect(port), "GET", port, "/api/dashboard", None, quick);
        assert_eq!(reply, Err(PostError::Timeout));
    }
}
