#![cfg(feature = "mqtt")]

use cls_eedi::transport::{
    mqtt::{Broker, Session, SessionEvent},
    Role, Routes,
};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Read a small fixture packet, including MQTT's variable-length size prefix.
fn packet(socket: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut byte = [0];
    socket.read_exact(&mut byte)?;
    let kind = byte[0];
    let mut len = 0usize;
    for shift in [0, 7, 14, 21] {
        socket.read_exact(&mut byte)?;
        len |= usize::from(byte[0] & 127) << shift;
        if byte[0] & 128 == 0 && len <= 4096 {
            let mut payload = vec![0; len];
            socket.read_exact(&mut payload)?;
            return Ok((kind, payload));
        }
        if len > 4096 {
            break;
        }
    }
    Err(io::Error::other("invalid fixture packet length"))
}

#[test]
fn poll_deadline_preserves_inflight_connect_on_the_original_socket() {
    let deadline = Duration::from_secs(3);
    let addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let listener = TcpListener::bind((addr, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (connected_tx, connected_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(deadline)).unwrap();
        assert_eq!(packet(&mut socket).unwrap().0, 0x10);
        connected_tx.send(()).unwrap();
        // Withhold CONNACK until the caller has returned from a deadline-limited poll.
        reply_rx.recv_timeout(deadline).unwrap();
        socket.write_all(&[0x20, 3, 0, 0, 0]).unwrap();
        let (kind, subscription) = packet(&mut socket).unwrap();
        assert_eq!(kind, 0x82);
        socket
            .write_all(&[0x90, 4, subscription[0], subscription[1], 0, 1])
            .unwrap();
        ready_rx.recv_timeout(deadline).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    });
    let broker = Broker::loopback("handshake", addr, port).unwrap();
    let routes = Routes::new("test/down", "test/up").unwrap();
    let mut session = Session::new(broker, Role::Backend, routes, "test").unwrap();
    let until = Instant::now() + deadline;
    loop {
        assert!(Instant::now() < until, "broker never received CONNECT");
        assert!(session.poll(Duration::from_millis(5)).unwrap().is_none());
        if connected_rx.try_recv().is_ok() {
            break;
        }
    }
    // No CONNACK is available; another short poll must yield without dropping CONNECT.
    assert!(session.poll(Duration::from_millis(1)).unwrap().is_none());
    reply_tx.send(()).unwrap();
    let until = Instant::now() + deadline;
    loop {
        assert!(Instant::now() < until, "never received SUBACK");
        if matches!(
            session.poll(Duration::from_millis(3)).unwrap(),
            Some(SessionEvent::Ready)
        ) {
            break;
        }
    }
    ready_tx.send(()).unwrap();
    peer.join().unwrap();
}
