//! Integration test: the audio path end-to-end through a live relay — the
//! host streams synthetic mic ticks (20 ms, 960 s16le samples each) as AUDIO
//! commands while also streaming a screen frame, proving both media types
//! coexist on record channel 0 and the audio arrives **byte-exact**.
//! This is the same live path `ovhost --mic` / `ovview` use.

use std::time::{Duration, Instant};

use ovapp::audio::{decode_audio_frame, encode_audio_frame, SineGen, TICK_SAMPLES};
use ovapp::screen::{synthetic_frame, Frame, PixelFormat};
use ovapp::terminal::{Command, TerminalSession};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

const W: u32 = 256;
const H: u32 = 128; // 256*128*4 = 128 KiB frame -> 16 chunks of 8 KiB
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

#[test]
fn audio_stream_over_relay_byte_exact_with_video() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let relay_addr = relay.local_addr();
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00CC_0001u64, 0x00DD_0002u64);
    let code = server_id.short_code();
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

    // Host (server) is the mic + screen source; view (client) is the sink.
    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);
    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);

    // Record the audio ticks the view's on_cmd hook observes.
    let seen_audio = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<i16>>::new()));
    let audio_hook = seen_audio.clone();
    view.on_cmd(Some(Box::new(move |cmd: &Command| {
        if let Command::Audio(payload) = cmd {
            // Every tick must decode cleanly and carry exactly one tick.
            let (_, samples) = decode_audio_frame(payload).expect("valid audio tick");
            assert_eq!(samples.len(), TICK, "one 20 ms tick per record");
            audio_hook.lock().unwrap().push(samples);
        }
    })));

    // Interleave audio ticks with one screen frame, like the real host loop.
    let mut mic = SineGen::new(440.0, 8000);
    let mut ticks: Vec<Vec<u8>> = Vec::new();
    for _ in 0..TICKS {
        let mut buf = [0i16; TICK];
        mic.tick(&mut buf);
        let payload = encode_audio_frame(&buf, 0);
        ticks.push(payload.clone());
        host.send(&Command::Audio(payload)).expect("send audio");
    }
    let frame = synthetic_frame(0, W, H);
    host.send_frame(&frame, 8 * 1024).expect("send frame");

    // Drain the view until every audio tick + the frame reassembled.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut got_ticks: Vec<Vec<i16>> = Vec::new();
    let mut got_frame: Option<Frame> = None;
    while (got_ticks.len() < TICKS || got_frame.is_none()) && Instant::now() < deadline {
        view.pump().expect("view pump");
        if got_ticks.len() < seen_audio.lock().unwrap().len() {
            got_ticks = seen_audio.lock().unwrap().clone();
        }
        if got_frame.is_none() {
            got_frame = view.take_frame();
        }
        if got_ticks.len() < TICKS || got_frame.is_none() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    assert_eq!(
        got_ticks.len(),
        TICKS,
        "all {} audio ticks must arrive over the relay",
        TICKS
    );
    // Byte-exact: decode(payload) == original sine tick, in order.
    for (idx, payload) in ticks.iter().enumerate() {
        let (_, samples) = decode_audio_frame(payload).expect("decode sent tick");
        assert_eq!(
            got_ticks[idx], samples,
            "audio tick {idx} must be byte-exact through the relay"
        );
    }

    // The screen frame must have reassembled intact alongside the audio.
    let f = got_frame.expect("screen frame must arrive alongside audio");
    assert_eq!(f.format, PixelFormat::Bgra);
    assert_eq!(f.width, W);
    assert_eq!(f.height, H);
    assert!(f.is_complete());
    assert_eq!(
        f.pixels, frame.pixels,
        "frame pixels must be byte-exact through the relay"
    );

    let h = host.stats();
    let v = view.stats();
    eprintln!("host stats: {h:?}");
    eprintln!("view stats: {v:?}");
    assert_eq!(h.decrypt_errors, 0, "host must not drop a record");
    assert_eq!(v.decrypt_errors, 0, "view must not drop a record");

    view.close().expect("close view");
    host.close().expect("close host");
}
