use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// A port is up when localhost accepts a TCP connection on IPv4 or IPv6.
pub fn port_is_open(port: u16) -> bool {
    let addresses = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    addresses
        .iter()
        .any(|address| TcpStream::connect_timeout(address, CONNECT_TIMEOUT).is_ok())
}

pub fn probe_ports(ports: impl IntoIterator<Item = u16>) -> BTreeMap<u16, bool> {
    let ports: Vec<u16> = ports.into_iter().collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = ports
            .into_iter()
            .map(|port| scope.spawn(move || (port, port_is_open(port))))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("port probe thread"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn open_listener_is_up_and_a_closed_port_is_down() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_is_open(port));
        drop(listener);
        assert!(!port_is_open(port));
    }
}
