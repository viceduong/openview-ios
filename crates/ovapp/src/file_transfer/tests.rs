use super::*;

fn data_cmds(file_id: u32, data: &[u8], chunk: usize) -> Vec<Command> {
    data_cmds_named(file_id, "test.bin", data, chunk)
}

fn data_cmds_named(file_id: u32, name: &str, data: &[u8], chunk: usize) -> Vec<Command> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    for c in data.chunks(chunk.max(1)) {
        out.push(Command::FileXferData {
            file_id,
            offset,
            chunk: c.to_vec(),
        });
        offset += c.len() as u64;
    }
    out.push(Command::FileXferComplete {
        file_id,
        total: data.len() as u64,
        ok: 1,
        name: name.to_owned(),
    });
    out
}

fn chunk(file_id: u32, offset: u64, bytes: &[u8]) -> Command {
    Command::FileXferData {
        file_id,
        offset,
        chunk: bytes.to_vec(),
    }
}

fn complete(file_id: u32, total: u64) -> Command {
    Command::FileXferComplete {
        file_id,
        total,
        ok: 1,
        name: String::new(),
    }
}

#[test]
fn roundtrip_small_file() {
    let mut tx = FileTransfer::new();
    let mut rx = FileTransfer::new();
    let payload = b"hello openview".to_vec();
    for cmd in tx.send_commands("small.bin", &payload, 0).expect("send") {
        rx.poll(&cmd).expect("poll");
    }
    let got = rx.take().expect("completed");
    assert_eq!(got.name, "small.bin");
    assert_eq!(got.data, payload);
    assert_eq!(rx.in_flight(), 0);
    assert_eq!(rx.transfers_rx(), 1);
}

#[test]
fn multi_chunk_file_larger_than_chunk_size() {
    let mut tx = FileTransfer::new();
    let mut rx = FileTransfer::new();
    let payload: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
    let cmds = tx.send_commands("big.bin", &payload, 4096).expect("send");
    // 50_000 / 4096 = 13 chunks (12 full + 1 partial) + completion.
    assert_eq!(cmds.len(), 14);
    assert!(matches!(cmds[0], Command::FileXferData { file_id: 1, .. }));
    for cmd in &cmds {
        rx.poll(cmd).expect("poll");
    }
    let got = rx.take().expect("completed");
    assert_eq!(got.data, payload);
    assert_eq!(rx.chunks_rx(), 13);
}

#[test]
fn two_concurrent_file_ids_interleaved() {
    let mut rx = FileTransfer::new();
    let a: Vec<u8> = (0..100u8).collect();
    let b: Vec<u8> = (200..250u8).collect();
    let cmds_a = data_cmds(7, &a, 30);
    let cmds_b = data_cmds(9, &b, 30);
    // Interleave the two streams chunk-by-chunk.
    let mut i = 0;
    while i < cmds_a.len().max(cmds_b.len()) {
        if let Some(c) = cmds_a.get(i) {
            rx.poll(c).expect("poll a");
        }
        if let Some(c) = cmds_b.get(i) {
            rx.poll(c).expect("poll b");
        }
        i += 1;
    }
    assert_eq!(rx.in_flight(), 0);
    let first = rx.take().expect("a completed");
    let second = rx.take().expect("b completed");
    // Completion order follows arrival order: b is shorter, so its
    // completion lands first even though a started first.
    assert_eq!((first.file_id, first.data), (9, b));
    assert_eq!((second.file_id, second.data), (7, a));
    assert_eq!(rx.transfers_rx(), 2);
}

#[test]
fn out_of_order_chunks_reassemble_by_offset() {
    let mut rx = FileTransfer::new();
    let payload: Vec<u8> = (0..60u8).collect();
    let mut cmds = data_cmds(3, &payload, 20);
    // Swap the first two data chunks (completion stays last).
    cmds.swap(0, 1);
    for cmd in &cmds {
        rx.poll(cmd).expect("poll");
    }
    assert_eq!(rx.take().expect("completed").data, payload);
}

#[test]
fn reversed_chunks_coalesce_into_one_fragment() {
    let mut rx = FileTransfer::new();
    let payload: Vec<u8> = (0..=255u8).collect();
    let mut cmds = data_cmds(3, &payload, 16);
    let done = cmds.pop().unwrap();
    for cmd in cmds.iter().rev() {
        rx.poll(cmd).expect("poll");
    }
    assert_eq!(rx.rx[&3].chunks.len(), 1, "fully coalesced");
    rx.poll(&done).expect("complete");
    assert_eq!(rx.take().unwrap().data, payload);
}

#[test]
fn truncated_transfer_accumulates_until_complete() {
    let mut rx = FileTransfer::new();
    let payload: Vec<u8> = (0..40u8).collect();
    let cmds = data_cmds(5, &payload, 10);
    // Feed only the data chunks — no completion yet.
    for cmd in cmds.iter().take(cmds.len() - 1) {
        rx.poll(cmd).expect("poll");
    }
    assert_eq!(rx.in_flight(), 1, "still accumulating");
    assert!(rx.take().is_none(), "not finalized without completion");
    // Now the completion arrives.
    rx.poll(cmds.last().expect("completion")).expect("poll");
    let got = rx.take().expect("completed");
    assert_eq!(got.data, payload);
}

#[test]
fn length_mismatch_is_an_error_not_a_panic() {
    let mut rx = FileTransfer::new();
    for c in data_cmds(2, b"0123456789", 5).iter().take(2) {
        rx.poll(c).expect("poll");
    }
    // Completion declares 99 bytes but only 10 were received.
    let err = rx
        .poll(&complete(2, 99))
        .expect_err("must reject the mismatch")
        .to_string();
    assert!(err.contains("declared"), "message: {err}");
    assert_eq!(rx.transfers_failed(), 1);
    assert!(rx.take().is_none());
}

#[test]
fn hole_is_rejected_even_when_the_length_matches() {
    let mut rx = FileTransfer::new();
    rx.poll(&chunk(1, 0, b"abc")).unwrap();
    rx.poll(&chunk(1, 10, b"xyz")).unwrap();
    let err = rx.poll(&complete(1, 6)).unwrap_err().to_string();
    assert!(err.contains("tile"), "{err}");
}

#[test]
fn duplicate_chunk_is_not_double_counted() {
    let mut rx = FileTransfer::new();
    rx.poll(&chunk(1, 0, b"hello")).unwrap();
    rx.poll(&chunk(1, 0, b"hello"))
        .expect("exact retransmit is ignored");
    rx.poll(&chunk(1, 2, b"ll"))
        .expect("retransmit inside a fragment");
    rx.poll(&chunk(1, 5, b" world")).unwrap();
    rx.poll(&complete(1, 11)).expect("11 bytes, not 18");
    assert_eq!(rx.take().unwrap().data, b"hello world");
}

#[test]
fn overlapping_chunk_rejects_the_transfer_once() {
    let mut rx = FileTransfer::new();
    rx.poll(&chunk(1, 0, b"hello")).unwrap();
    let err = rx.poll(&chunk(1, 3, b"XXXX")).unwrap_err().to_string();
    assert!(err.contains("overlaps"), "{err}");
    // Later chunks + completion of the dropped transfer: silent.
    rx.poll(&chunk(1, 9, b"zz")).expect("silently dropped");
    rx.poll(&complete(1, 11)).expect("silently dropped");
    assert!(rx.take().is_none());
    assert_eq!(rx.in_flight(), 0);
    // Overlap with the *next* fragment is caught too.
    rx.poll(&chunk(2, 10, b"abc")).unwrap();
    assert!(rx.poll(&chunk(2, 8, b"0123")).is_err());
    // A differing chunk at a known offset is an overlap, not a duplicate.
    rx.poll(&chunk(3, 0, b"abc")).unwrap();
    assert!(rx.poll(&chunk(3, 0, b"abd")).is_err());
}

#[test]
fn concurrent_transfer_cap() {
    let mut rx = FileTransfer::with_limits(TransferLimits {
        max_concurrent: 2,
        ..TransferLimits::default()
    });
    rx.poll(&chunk(1, 0, b"a")).unwrap();
    rx.poll(&chunk(2, 0, b"b")).unwrap();
    let err = rx.poll(&chunk(3, 0, b"c")).unwrap_err().to_string();
    assert!(err.contains("concurrent"), "{err}");
    rx.poll(&complete(1, 1)).unwrap();
    rx.poll(&chunk(4, 0, b"d")).expect("a slot freed up");
}

#[test]
fn in_flight_byte_budget() {
    let mut rx = FileTransfer::with_limits(TransferLimits {
        max_in_flight_bytes: 10,
        ..TransferLimits::default()
    });
    rx.poll(&chunk(1, 0, &[0; 6])).unwrap();
    let err = rx.poll(&chunk(2, 0, &[0; 6])).unwrap_err().to_string();
    assert!(err.contains("budget"), "{err}");
    // Completed-but-untaken files still count.
    rx.poll(&complete(1, 6)).unwrap();
    assert!(rx.poll(&chunk(3, 0, &[0; 6])).is_err());
    let _ = rx.take();
    rx.poll(&chunk(4, 0, &[0; 6]))
        .expect("budget freed by take()");
}

#[test]
fn per_file_cap_and_offset_overflow() {
    let mut rx = FileTransfer::with_limits(TransferLimits {
        max_file_bytes: 100,
        ..TransferLimits::default()
    });
    let err = rx.poll(&chunk(1, 99, b"ab")).unwrap_err().to_string();
    assert!(err.contains("per-file"), "{err}");
    assert!(rx.poll(&chunk(2, u64::MAX, b"x")).is_err());
    assert!(rx.poll(&chunk(3, 50, b"")).is_err(), "empty chunk mid-file");
}

#[test]
fn fragment_cap() {
    let mut rx = FileTransfer::new();
    for i in 0..MAX_FRAGMENTS as u64 {
        rx.poll(&chunk(1, i * 2, b"x")).unwrap();
    }
    let err = rx
        .poll(&chunk(1, MAX_FRAGMENTS as u64 * 2, b"x"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("fragments"), "{err}");
}

#[test]
fn complete_for_unknown_file_id_is_an_error() {
    let mut rx = FileTransfer::new();
    let err = rx
        .poll(&complete(42, 0))
        .expect_err("unknown file id must error")
        .to_string();
    assert!(err.contains("unknown"), "message: {err}");
}

#[test]
fn abort_drops_the_in_flight_transfer() {
    let mut rx = FileTransfer::new();
    for c in data_cmds(4, b"partial data here", 6).iter().take(2) {
        rx.poll(c).expect("poll");
    }
    assert_eq!(rx.in_flight(), 1);
    rx.poll(&Command::FileXferComplete {
        file_id: 4,
        total: 17,
        ok: 0,
        name: String::new(),
    })
    .expect("abort is not an error");
    assert_eq!(rx.in_flight(), 0);
    assert!(rx.take().is_none());
    assert_eq!(rx.transfers_failed(), 1);
}

#[test]
fn empty_file_transfers_as_completion_only() {
    let mut tx = FileTransfer::new();
    let mut rx = FileTransfer::new();
    let cmds = tx.send_commands("empty.bin", b"", 0).expect("send");
    assert_eq!(cmds.len(), 2, "one zero-length chunk + completion");
    for cmd in &cmds {
        rx.poll(cmd).expect("poll");
    }
    let got = rx.take().expect("completed");
    assert_eq!(got.name, "empty.bin");
    assert!(got.data.is_empty());
}

#[test]
fn send_allocates_distinct_file_ids() {
    let mut tx = FileTransfer::new();
    let c1 = tx.send_commands("a", b"x", 0).expect("send");
    let c2 = tx.send_commands("b", b"y", 0).expect("send");
    let id1 = match &c1[0] {
        Command::FileXferData { file_id, .. } => *file_id,
        _ => panic!("first command must be a data chunk"),
    };
    let id2 = match &c2[0] {
        Command::FileXferData { file_id, .. } => *file_id,
        _ => panic!("first command must be a data chunk"),
    };
    assert_ne!(id1, id2, "concurrent transfers need distinct ids");
}

#[test]
fn non_file_commands_are_ignored() {
    let mut rx = FileTransfer::new();
    rx.poll(&Command::Ping(7)).expect("ignored");
    rx.poll(&Command::Echo(b"hi".to_vec())).expect("ignored");
    assert_eq!(rx.in_flight(), 0);
    assert_eq!(rx.chunks_rx(), 0);
}

#[test]
fn sanitize_name_strips_paths_and_hostile_names() {
    let s = FileTransfer::sanitize_name;
    assert_eq!(s("a/b/c.txt"), "c.txt");
    assert_eq!(s("..\\evil.exe"), "evil.exe");
    assert_eq!(s("C:/tmp/x"), "x");
    assert_eq!(s(".."), "_");
    assert_eq!(s("."), "_");
    assert_eq!(s(""), "_");
    assert_eq!(s("ok\u{1}name"), "ok_name");
    // Drive-relative paths and alternate data streams.
    assert_eq!(s("D:evil.exe"), "D_evil.exe");
    assert_eq!(s("x.txt:ads"), "x.txt_ads");
    assert_eq!(s("C:"), "C_");
    assert_eq!(s("\\\\server\\share\\f.txt"), "f.txt");
    assert_eq!(s("\\\\?\\C:\\Windows\\x.dll"), "x.dll");
    // Windows-invalid characters.
    assert_eq!(s("a<b>c|d?e*f\"g"), "a_b_c_d_e_f_g");
    // Trailing dots / spaces are stripped by Windows; strip them first.
    assert_eq!(s("evil.exe. . "), "evil.exe");
    assert_eq!(s("..."), "_");
    assert_eq!(s("  spaced.txt"), "spaced.txt");
    // Reserved device names, any case / extension.
    assert_eq!(s("CON"), "_CON");
    assert_eq!(s("nul.txt"), "_nul.txt");
    assert_eq!(s("Com1.log"), "_Com1.log");
    assert_eq!(s("lpt9"), "_lpt9");
    assert_eq!(s("COM¹"), "_COM¹");
    assert_eq!(s("console.txt"), "console.txt");
    assert_eq!(s("com10"), "com10");
    // Unicode survives; length is capped with the extension kept.
    assert_eq!(s("résumé.pdf"), "résumé.pdf");
    let long = format!("{}.txt", "é".repeat(300));
    let out = s(&long);
    assert!(out.len() <= 255 && out.ends_with(".txt"), "{}", out.len());
}

#[test]
fn sanitized_names_are_always_a_single_normal_component() {
    use std::path::{Component, Path};
    for name in [
        "a/b",
        "..",
        "../../etc/passwd",
        "C:\\x",
        "D:y",
        "\\\\srv\\s\\z",
        "/abs",
        "x:y:z",
        "",
        " . ",
    ] {
        let out = FileTransfer::sanitize_name(name);
        let comps: Vec<_> = Path::new(&out).components().collect();
        assert_eq!(comps.len(), 1, "{name:?} -> {out:?}");
        assert!(
            matches!(comps[0], Component::Normal(_)),
            "{name:?} -> {out:?}"
        );
        assert!(!out.contains(':'));
    }
}

#[test]
fn unique_name_appends_counters() {
    use std::collections::HashSet;
    let taken: HashSet<&str> = ["a.txt", "a (1).txt", "README"].into();
    let exists = |n: &str| taken.contains(n);
    assert_eq!(unique_name("b.txt", exists).unwrap(), "b.txt");
    assert_eq!(unique_name("a.txt", exists).unwrap(), "a (2).txt");
    assert_eq!(unique_name("README", exists).unwrap(), "README (1)");
    assert_eq!(unique_name("x", |_| true), None);
    let long = "x".repeat(255);
    let u = unique_name(&long, |n| n == long).unwrap();
    assert!(u.len() <= 255 && u.ends_with(" (1)"), "{u}");
}

#[test]
fn wire_roundtrip_through_encode_decode() {
    // The commands survive the terminal layer's encode/decode intact.
    let mut tx = FileTransfer::new();
    let mut rx = FileTransfer::new();
    let payload: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    for cmd in tx.send_commands("wire.bin", &payload, 1000).expect("send") {
        let wire = crate::terminal::encode(&cmd);
        let decoded = crate::terminal::decode(&wire).expect("decode");
        rx.poll(&decoded).expect("poll");
    }
    assert_eq!(rx.take().expect("completed").data, payload);
}
