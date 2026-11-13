//! Integration test: **bidirectional** (full-duplex) audio over a live relay.
//! The AUDIO (0x40) opcode is direction-agnostic — the sender of a
//! record is always "my mic", the receiver plays it as "the remote mic". Two
//! endpoints each stream a *distinct* synthetic tone as 0x40 ticks
//! concurrently; each side must receive the OTHER's audio, decodable and
//! non-empty, proving the channel carries audio in both directions at once.
//! This is the same live path `ovhost --audio` / `ovview --mic` exercise.

use std::time::{Duration, Instant};

use ovapp::audio::{decode_audio_frame, encode_audio_frame, SineGen, TICK_SAMPLES};
use ovapp::terminal::{Command, TerminalSession};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

const TICK: usize = TICK_SAMPLES as usize;
const TICKS: usize = 10;

/// Receive datagrams until one arrives (deadline-bounded), feeding `hs`.
fn flush(hs: &mut Handshake, carrier: &mut RelayTunnelCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("relay send");
    }
}

fn pump_until_established(
    client: &mut Handshake,
    server: &mut Handshake,
    cc: &mut RelayTunnelCarrier,
    sc: &mut RelayTunnelCarrier,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(client.is_established() && server.is_established()) && Instant::now() < deadline {
        sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        })
        .expect("relay poll");
        flush(server, sc);
        cc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        })
        .expect("relay poll");
        flush(client, cc);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Which tone each side listens for (the OTHER side's frequency).
const FREQ_A: f32 = 440.0; // endpoint A's mic tone
const FREQ_B: f32 = 660.0; // endpoint B's mic tone

#[test]
fn audio_is_full_duplex_over_relay_both_directions() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let relay_addr = relay.local_addr();
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00CC_0001u64, 0x00DD_0002u64);
    let code = server_id.pairing_code();
    let session_id = u32::from_be_bytes(code.as_bytes()[..4].try_into().expect("code >= 4"));

    let mut cc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Initiator)
        .expect("view connect");
    let mut sc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Responder)
        .expect("host connect");

    let mut client = Handshake::new(
        Role::Client,
        dev_c,
        &client_id,
        Auth::PairingCode(code.clone()),
    );
    client.expect_fingerprint(server_id.fingerprint());
    let mut server = Handshake::new(Role::Server, dev_s, &server_id, Auth::PairingCode(code));
    server.expect_device(dev_c);

    flush(&mut client, &mut cc);
    pump_until_established(&mut client, &mut server, &mut cc, &mut sc);
    assert!(client.is_established(), "client: {:?}", client.failed());
    assert!(server.is_established(), "server: {:?}", server.failed());

    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);
    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);

    // Each side records the audio ticks it RECEIVES (the remote mic).
    let seen_at_host = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<i16>>::new()));
    let seen_at_view = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<i16>>::new()));
    {
        let hook = seen_at_host.clone();
        host.on_cmd(Some(Box::new(move |cmd: &Command| {
            if let Command::Audio(payload) = cmd {
                let (_, samples) = decode_audio_frame(payload).expect("valid audio tick");
                assert_eq!(samples.len(), TICK, "one 20 ms tick per record");
                hook.lock().unwrap().push(samples);
            }
        })));
    }
    {
        let hook = seen_at_view.clone();
        view.on_cmd(Some(Box::new(move |cmd: &Command| {
            if let Command::Audio(payload) = cmd {
                let (_, samples) = decode_audio_frame(payload).expect("valid audio tick");
                assert_eq!(samples.len(), TICK, "one 20 ms tick per record");
                hook.lock().unwrap().push(samples);
            }
        })));
    }

    // Both ends capture concurrently: A sends 440 Hz, B sends 660 Hz —
    // exactly what `ovhost --mic` + `ovview --mic` do in a full-duplex
    // session.
    let mut mic_a = SineGen::new(FREQ_A, 8_000);
    let mut mic_b = SineGen::new(FREQ_B, 8_000);
    let mut tick_a = [0i16; TICK];
    let mut tick_b = [0i16; TICK];
    for _ in 0..TICKS {
        mic_a.tick(&mut tick_a);
        mic_b.tick(&mut tick_b);
        host.send(&Command::Audio(encode_audio_frame(&tick_a, 0)))
            .expect("host send audio");
        view.send(&Command::Audio(encode_audio_frame(&tick_b, 0)))
            .expect("view send audio");
    }

    // Drain both sides until each has seen TICKS ticks of the OTHER's mic.
    let deadline = Instant::now() + Duration::from_secs(6);
    while (seen_at_host.lock().unwrap().len() < TICKS || seen_at_view.lock().unwrap().len() < TICKS)
        && Instant::now() < deadline
    {
        host.pump().expect("host pump");
        view.pump().expect("view pump");
        if seen_at_host.lock().unwrap().len() < TICKS || seen_at_view.lock().unwrap().len() < TICKS
        {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    let at_host = seen_at_host.lock().unwrap().clone();
    let at_view = seen_at_view.lock().unwrap().clone();
    assert_eq!(
        at_host.len(),
        TICKS,
        "host must receive all {} of the viewer's mic ticks",
        TICKS
    );
    assert_eq!(
        at_view.len(),
        TICKS,
        "viewer must receive all {} of the host's mic ticks",
        TICKS
    );

    // Regenerate each side's exact capture stream and verify the received
    // ticks are byte-exact AND the other side's tone (no echo of our own
    // mic): the host hears B's 660 Hz, the viewer hears A's 440 Hz.
    let mut gen_a = SineGen::new(FREQ_A, 8_000);
    let mut gen_b = SineGen::new(FREQ_B, 8_000);
    let mut exp_a: Vec<Vec<i16>> = Vec::with_capacity(TICKS);
    let mut exp_b: Vec<Vec<i16>> = Vec::with_capacity(TICKS);
    for _ in 0..TICKS {
        let mut a = [0i16; TICK];
        let mut b = [0i16; TICK];
        gen_a.tick(&mut a);
        gen_b.tick(&mut b);
        exp_a.push(a.to_vec());
        exp_b.push(b.to_vec());
    }
    assert_eq!(
        at_host, exp_b,
        "host must receive the viewer's 660 Hz mic, byte-exact in order"
    );
    assert_eq!(
        at_view, exp_a,
        "viewer must receive the host's 440 Hz mic, byte-exact in order"
    );
    // The received streams really are the OTHER's tone, not our own echo.
    assert_ne!(exp_a[0], exp_b[0], "the two tones must be distinguishable");

    let h = host.stats();
    let v = view.stats();
    eprintln!("host stats: {h:?}");
    eprintln!("view stats: {v:?}");
    assert_eq!(h.decrypt_errors, 0, "host must not drop a record");
    assert_eq!(v.decrypt_errors, 0, "view must not drop a record");

    view.close().expect("close view");
    host.close().expect("close host");
}
