//! Integration test: the end-to-end "money test" — two full `TerminalSession`s
//! (host + view) through a complete ovcore handshake over an in-memory
//! carrier pair, then ECHO round-trip, PING->PONG, a byte-exact file
//! transfer, and a chat line both ways.

use std::time::Duration;

use ovapp::chat::ChatSession;
use ovapp::terminal::{Command, TerminalSession};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, InMemoryCarrier};

/// Drain a handshake's outgoing datagrams into the carrier.
fn flush(hs: &mut Handshake, carrier: &mut InMemoryCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("inmem send");
    }
}

/// Deliver every queued datagram from `src` into `dst_hs`.
fn pump(src: &mut InMemoryCarrier, dst_hs: &mut Handshake) {
    src.poll_recv(&mut |dgram| {
        dst_hs.feed(dgram).expect("handshake feed");
    })
    .expect("inmem poll_recv");
}

/// Establish a session pair over an in-memory carrier pair. Returns the two
/// terminal sessions (client, server) with their carriers attached.
fn establish_pair() -> (TerminalSession, TerminalSession) {
    let (mut cc, mut sc) = InMemoryCarrier::pair();
    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0001u64, 0x00BB_0002u64);

    let server_code = server_id.short_code();
    let mut client = Handshake::new(
        Role::Client,
        dev_c,
        &client_id,
        Auth::PairingCode(server_code.clone()),
    );
    client.expect_fingerprint(server_id.fingerprint());
    let mut server = Handshake::new(
        Role::Server,
        dev_s,
        &server_id,
        Auth::PairingCode(server_code),
    );
    server.expect_device(dev_c);

    // Client HELLO is emitted at construction; in-memory is lossless so the
    // three flights converge immediately.
    flush(&mut client, &mut cc);
    pump(&mut sc, &mut server);
    flush(&mut server, &mut sc);
    pump(&mut cc, &mut client);
    // Pairing codes are a PAKE: the client's CONFIRM completes the server.
    flush(&mut client, &mut cc);
    pump(&mut sc, &mut server);

    assert!(client.is_established(), "client: {:?}", client.failed());
    assert!(server.is_established(), "server: {:?}", server.failed());
    assert_eq!(client.peer().unwrap().fingerprint, server_id.fingerprint());
    assert_eq!(server.peer().unwrap().fingerprint, client_id.fingerprint());

    let client_session = client.take_session().expect("client session");
    let server_session = server.take_session().expect("server session");
    (
        TerminalSession::new(client_session, cc),
        TerminalSession::new(server_session, sc),
    )
}

#[test]
fn echo_ping_file_and_chat_roundtrip() {
    let (mut view, mut host) = establish_pair();

    // --- ECHO round-trip ---
    view.send(&Command::Echo(b"hello host".to_vec()))
        .expect("send echo");
    let reply = host
        .wait_for(Duration::from_secs(5), |cmd| match cmd {
            Command::Echo(p) => Some(p.clone()),
            _ => None,
        })
        .expect("host saw ECHO");
    assert_eq!(reply, b"hello host".to_vec());
    // The host's pump auto-replies ECHO; the view must see the same payload.
    let echoed = view
        .wait_for(Duration::from_secs(5), |cmd| match cmd {
            Command::Echo(p) => Some(p.clone()),
            _ => None,
        })
        .expect("view saw ECHO reply");
    assert_eq!(echoed, b"hello host".to_vec());

    // --- PING -> PONG ---
    view.send(&Command::Ping(0xDEAD_BEEF)).expect("send ping");
    // The host auto-replies PONG inside its pump.
    host.pump().expect("host pump (auto-pong)");
    let pong = view
        .wait_for(Duration::from_secs(5), |cmd| match cmd {
            Command::Pong(n) => Some(*n),
            _ => None,
        })
        .expect("view saw PONG");
    assert_eq!(pong, 0xDEAD_BEEF);

    // --- File transfer, byte-exact reassembly ---
    let payload: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    view.send_file("notes.bin", &payload, 4096)
        .expect("send file");
    // Host side: pump until the FILE_END-driven reassembly lands.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while got.is_none() && std::time::Instant::now() < deadline {
        host.pump().expect("host pump");
        got = host.take_file();
        if got.is_none() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let (name, data) = got.expect("file reassembled");
    assert_eq!(name, "notes.bin");
    assert_eq!(data, payload, "file must reassemble byte-exact");

    // --- Chat lines both ways ---
    let (mut cv, mut ch) = {
        // Chat is a thin wrapper; build a second pair on fresh carriers by
        // re-establishing (cheap: the handshake is ~1 ms on loopback).
        establish_pair_chat()
    };
    cv.send_line("hello from view").expect("chat send v->h");
    let line = ch
        .wait_for_line(Duration::from_secs(5))
        .expect("host got line");
    assert_eq!(line, "hello from view");
    ch.send_line("hello from host").expect("chat send h->v");
    let line = cv
        .wait_for_line(Duration::from_secs(5))
        .expect("view got line");
    assert_eq!(line, "hello from host");

    cv.close().unwrap();
    ch.close().unwrap();
    view.close().unwrap();
    host.close().unwrap();
}

/// Same as `establish_pair` but wrapped in chat sessions (the handshake pump
/// is identical; only the outer wrapper differs).
fn establish_pair_chat() -> (ChatSession, ChatSession) {
    let (mut cc, mut sc) = InMemoryCarrier::pair();
    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0003u64, 0x00BB_0004u64);

    let server_code = server_id.short_code();
    let mut client = Handshake::new(
        Role::Client,
        dev_c,
        &client_id,
        Auth::PairingCode(server_code.clone()),
    );
    client.expect_fingerprint(server_id.fingerprint());
    let mut server = Handshake::new(
        Role::Server,
        dev_s,
        &server_id,
        Auth::PairingCode(server_code),
    );
    server.expect_device(dev_c);

    flush(&mut client, &mut cc);
    pump(&mut sc, &mut server);
    flush(&mut server, &mut sc);
    pump(&mut cc, &mut client);
    // Pairing codes are a PAKE: the client's CONFIRM completes the server.
    flush(&mut client, &mut cc);
    pump(&mut sc, &mut server);

    let client_session = client.take_session().expect("client session");
    let server_session = server.take_session().expect("server session");
    (
        ChatSession::new(client_session, cc),
        ChatSession::new(server_session, sc),
    )
}
