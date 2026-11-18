//! Probing a remote machine's ports through WhatsUp's SSH SOCKS tunnel.
//!
//! A forwarded port always accepts a connection, so "is the local port open"
//! says nothing about the remote server. Instead each probe asks the tunnel to
//! CONNECT to the link's own host and port; the remote sshd resolves the name
//! and connects exactly as the browser's request will. OpenSSH holds its
//! success reply until the remote connect worked, and closes the socket
//! without a reply when it failed.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::http::exchange;
pub use crate::http::PostError;

/// How long each stage (connect, greeting, reply) may take.
pub const REMOTE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The remote machine connected to the target.
    Up,
    /// The tunnel works, but nothing answered on the target.
    Down,
    /// The tunnel itself is not usable (not running, stalled, not SOCKS).
    Offline,
}

pub fn socks_reach(socks_port: u16, host: &str, port: u16, timeout: Duration) -> Reach {
    match socks_connect(socks_port, host, port, timeout) {
        Ok(_) => Reach::Up,
        Err(reach) => reach,
    }
}

/// Opens a connection to `host:port` as seen from the remote machine. The
/// error is `Down` (the remote refused) or `Offline` (the tunnel is unusable).
pub fn socks_connect(
    socks_port: u16,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, Reach> {
    let Ok(host_len) = u8::try_from(host.len()) else {
        return Err(Reach::Down);
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, socks_port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
        return Err(Reach::Offline);
    };
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return Err(Reach::Offline);
    }
    let mut greeting = [0u8; 2];
    if stream.write_all(&[5, 1, 0]).is_err()
        || stream.read_exact(&mut greeting).is_err()
        || greeting != [5, 0]
    {
        return Err(Reach::Offline);
    }
    let mut request = vec![5, 1, 0, 3, host_len];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    if stream.write_all(&request).is_err() {
        return Err(Reach::Offline);
    }
    // The reply is 4 bytes plus a bound address whose length depends on its
    // type; read all of it so the stream is left at the payload.
    let mut reply = [0u8; 4];
    match stream.read_exact(&mut reply) {
        Ok(()) if reply[1] == 0 => {}
        Ok(()) => return Err(Reach::Down),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset
            ) =>
        {
            return Err(Reach::Down)
        }
        Err(_) => return Err(Reach::Offline),
    }
    let address_len = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            if stream.read_exact(&mut len).is_err() {
                return Err(Reach::Offline);
            }
            len[0] as usize
        }
        _ => return Err(Reach::Offline),
    };
    let mut bound = vec![0u8; address_len + 2];
    if stream.read_exact(&mut bound).is_err() {
        return Err(Reach::Offline);
    }
    Ok(stream)
}

/// POSTs a JSON `body` to the WhatsUp listening on the remote machine's
/// loopback `port`, through the tunnel, and returns the status and body of
/// its reply. The connection is used once (`Connection: close`).
pub fn post_json(
    socks_port: u16,
    port: u16,
    path: &str,
    body: &str,
    read_timeout: Duration,
) -> Result<(u16, String), PostError> {
    let stream = socks_connect(socks_port, "127.0.0.1", port, REMOTE_TIMEOUT)
        .map_err(|_| PostError::Connect)?;
    exchange(stream, "POST", port, path, Some(body), read_timeout)
}

/// Probes every distinct (host, port) through the tunnel at once.
pub fn probe_remote(
    socks_port: u16,
    targets: impl IntoIterator<Item = (String, u16)>,
) -> BTreeMap<(String, u16), Reach> {
    let targets: std::collections::BTreeSet<(String, u16)> = targets.into_iter().collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .into_iter()
            .map(|target| {
                scope.spawn(move || {
                    let reach = socks_reach(socks_port, &target.0, target.1, REMOTE_TIMEOUT);
                    (target, reach)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("remote probe thread"))
            .collect()
    })
}

#[cfg(test)]
pub mod fake {
    //! A tiny SOCKS5 server for tests that answers each CONNECT as told.
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    #[derive(Debug, Clone, Copy)]
    pub enum Act {
        Reply(u8),
        Close,
        Silent,
        BadGreeting,
        /// Succeed, then carry bytes to and from the real `host:port`.
        Relay,
    }

    /// Starts the server; `act` decides per (host, port). Returns its port and
    /// a receiver of the raw CONNECT requests it read.
    pub fn start(act: fn(&str, u16) -> Act) -> (u16, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let tx = tx.clone();
                thread::spawn(move || serve(stream, act, tx));
            }
        });
        (port, rx)
    }

    fn serve(mut stream: TcpStream, act: fn(&str, u16) -> Act, tx: mpsc::Sender<Vec<u8>>) {
        let mut greeting = [0u8; 3];
        if stream.read_exact(&mut greeting).is_err() {
            return;
        }
        if matches!(act("", 0), Act::BadGreeting) {
            let _ = stream.write_all(&[5, 0xFF]);
            return;
        }
        let _ = stream.write_all(&[5, 0]);
        let mut head = [0u8; 5];
        if stream.read_exact(&mut head).is_err() {
            return;
        }
        let mut rest = vec![0u8; head[4] as usize + 2];
        if stream.read_exact(&mut rest).is_err() {
            return;
        }
        let host = String::from_utf8_lossy(&rest[..head[4] as usize]).to_string();
        let port = u16::from_be_bytes([rest[rest.len() - 2], rest[rest.len() - 1]]);
        let mut request = head.to_vec();
        request.extend_from_slice(&rest);
        let _ = tx.send(request);
        match act(&host, port) {
            Act::Reply(code) => {
                let _ = stream.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]);
                let mut sink = [0u8; 1];
                let _ = stream.read(&mut sink);
            }
            Act::Close | Act::BadGreeting => {}
            Act::Relay => {
                let Ok(upstream) = TcpStream::connect((host.as_str(), port)) else {
                    return;
                };
                let _ = stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
                let (mut from_client, mut to_upstream) =
                    (stream.try_clone().unwrap(), upstream.try_clone().unwrap());
                thread::spawn(move || {
                    let _ = std::io::copy(&mut from_client, &mut to_upstream);
                    let _ = to_upstream.shutdown(std::net::Shutdown::Write);
                });
                let (mut from_upstream, mut to_client) = (upstream, stream);
                let _ = std::io::copy(&mut from_upstream, &mut to_client);
                let _ = to_client.shutdown(std::net::Shutdown::Both);
            }
            Act::Silent => thread::sleep(std::time::Duration::from_secs(5)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{self, Act};
    use super::*;
    use std::time::{Duration, Instant};

    const QUICK: Duration = Duration::from_millis(200);

    #[test]
    fn success_reply_is_up() {
        let (port, requests) = fake::start(|_, _| Act::Reply(0));
        assert_eq!(socks_reach(port, "jds4.local.me", 3004, QUICK), Reach::Up);
        let mut expected = vec![5, 1, 0, 3, 13];
        expected.extend_from_slice(b"jds4.local.me");
        expected.extend_from_slice(&3004u16.to_be_bytes());
        assert_eq!(requests.recv().unwrap(), expected);
    }

    #[test]
    fn close_without_reply_is_down() {
        let (port, _requests) = fake::start(|_, _| Act::Close);
        assert_eq!(socks_reach(port, "localhost", 4004, QUICK), Reach::Down);
    }

    #[test]
    fn failure_reply_is_down() {
        let (port, _requests) = fake::start(|_, _| Act::Reply(5));
        assert_eq!(socks_reach(port, "localhost", 4004, QUICK), Reach::Down);
    }

    #[test]
    fn refused_is_offline() {
        // Nothing listens on port 1, and parallel tests cannot take it.
        assert_eq!(socks_reach(1, "localhost", 4004, QUICK), Reach::Offline);
    }

    #[test]
    fn silent_server_is_offline() {
        let (port, _requests) = fake::start(|_, _| Act::Silent);
        let started = Instant::now();
        assert_eq!(socks_reach(port, "localhost", 4004, QUICK), Reach::Offline);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bad_greeting_is_offline() {
        let (port, _requests) = fake::start(|_, _| Act::BadGreeting);
        assert_eq!(socks_reach(port, "localhost", 4004, QUICK), Reach::Offline);
    }

    /// Answers one HTTP request with `status` and `body`, and hands back
    /// what it was sent.
    fn one_shot_http(status: u16, body: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut chunk = [0u8; 1024];
            // The request body here is short JSON ending in '}'.
            while !seen.ends_with(b"}") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..read]);
            }
            let _ = tx.send(String::from_utf8_lossy(&seen).to_string());
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
        });
        (port, rx)
    }

    #[test]
    fn post_json_through_relay() {
        let (socks, _requests) = fake::start(|_, _| Act::Relay);
        let (port, seen) = one_shot_http(409, r#"{"error":"already stopping"}"#);
        let reply = post_json(socks, port, "/api/stop", r#"{"row":3}"#, QUICK * 10);
        assert_eq!(
            reply,
            Ok((409, r#"{"error":"already stopping"}"#.to_string()))
        );
        let request = seen.recv().unwrap();
        assert!(
            request.starts_with("POST /api/stop HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
            "{request}"
        );
        assert!(request.contains("Connection: close\r\n"), "{request}");
        assert!(
            !request.to_ascii_lowercase().contains("origin:"),
            "{request}"
        );
        assert!(request.ends_with(r#"{"row":3}"#), "{request}");
    }

    #[test]
    fn post_json_fails_without_a_listener() {
        let (socks, _requests) = fake::start(|_, _| Act::Close);
        let post = |socks| post_json(socks, 47654, "/api/stop", "{}", QUICK);
        assert_eq!(post(socks), Err(PostError::Connect));
        assert_eq!(post(1), Err(PostError::Connect));
        let (silent, _requests) = fake::start(|_, _| Act::Reply(0));
        assert!(matches!(post(silent), Err(PostError::Reply(_))));
    }

    #[test]
    fn probe_remote_dedups_and_maps_every_target() {
        let (port, _requests) = fake::start(|_, port| {
            if port == 3000 {
                Act::Reply(0)
            } else {
                Act::Close
            }
        });
        let status = probe_remote(
            port,
            [
                ("localhost".to_string(), 3000),
                ("localhost".to_string(), 3000),
                ("jds.local.me".to_string(), 8000),
            ],
        );
        assert_eq!(status.len(), 2);
        assert_eq!(status[&("localhost".to_string(), 3000)], Reach::Up);
        assert_eq!(status[&("jds.local.me".to_string(), 8000)], Reach::Down);
        assert_eq!(REMOTE_TIMEOUT, Duration::from_millis(1500));
    }
}
