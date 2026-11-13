//! Keeps the hand-written C header (`ios/OvFFI/include/shim.h`) in sync with
//! the exported symbols: every `#[no_mangle] extern "C" fn` must be declared
//! in the header and every declared `ov_*` function must exist here, the
//! `ov_video_packet` fields must match `OvVideoPacket` in order, and the
//! shared constants must agree.

use std::collections::BTreeSet;

const HEADER: &str = include_str!("../../../../ios/OvFFI/include/shim.h");
/// Every non-test source file of the crate (the entry points live in
/// several modules).
const SOURCE: &str = concat!(
    include_str!("../lib.rs"),
    include_str!("../cabi.rs"),
    include_str!("../callbacks.rs"),
    include_str!("../direct.rs"),
    include_str!("../error.rs"),
    include_str!("../file_transfer.rs"),
    include_str!("../identity.rs"),
    include_str!("../media.rs"),
    include_str!("../pump.rs"),
    include_str!("../relay.rs"),
    include_str!("../session.rs"),
    include_str!("../transport.rs"),
    include_str!("../screen/mod.rs"),
    include_str!("../screen/feed.rs"),
    include_str!("../screen/framebuffer.rs"),
    include_str!("../screen/liveness.rs"),
    include_str!("../screen/sinks.rs"),
    include_str!("../screen/video.rs"),
);

/// Strip `//` and `/* */` comments so commented-out names never count.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("//") {
            rest = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            let ch = rest.chars().next().unwrap_or_default();
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

/// The identifier ending right before byte offset `end` of `text`.
fn ident_before(text: &str, end: usize) -> &str {
    let head = text[..end].trim_end();
    let start = head
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map_or(0, |i| i + 1);
    &head[start..]
}

/// Function names declared in the header (`... ov_name(` outside typedefs).
fn header_functions() -> BTreeSet<String> {
    let text: String = strip_comments(HEADER)
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut names = BTreeSet::new();
    for stmt in text.split(';') {
        let stmt = stmt.trim();
        if stmt.contains("typedef") {
            continue;
        }
        if let Some(paren) = stmt.find('(') {
            let name = ident_before(stmt, paren);
            if name.starts_with("ov_") {
                names.insert(name.to_owned());
            }
        }
    }
    names
}

/// Exported entry points: `#[no_mangle]` followed by an `extern "C" fn`.
fn exported_functions() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut lines = SOURCE.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "#[no_mangle]" {
            continue;
        }
        let Some(sig) = lines.next() else { break };
        if let Some(after) = sig.split("extern \"C\" fn ").nth(1) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            names.insert(name);
        }
    }
    names
}

/// Field names, in order, of the block that starts at `opener`.
fn fields_after(
    text: &str,
    opener: &str,
    field_of: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let start = text.find(opener).expect("struct opener not found") + opener.len();
    let body = &text[start..];
    let end = body.find('}').expect("struct end not found");
    body[..end]
        .lines()
        .filter_map(|l| field_of(l.trim()))
        .collect()
}

/// `#define NAME value` from the header, as an integer.
fn header_define(name: &str) -> i64 {
    let text = strip_comments(HEADER);
    let line = text
        .lines()
        .find(|l| {
            l.split_whitespace().nth(1) == Some(name) && l.trim_start().starts_with("#define")
        })
        .unwrap_or_else(|| panic!("#define {name} missing from shim.h"));
    let value = line.split_whitespace().nth(2).expect("define value");
    value
        .trim_matches(|c| c == '(' || c == ')')
        .trim_end_matches('u')
        .parse()
        .expect("integer define")
}

#[test]
fn header_declares_exactly_the_exported_functions() {
    let header = header_functions();
    let exported = exported_functions();
    assert!(
        exported.len() > 40,
        "export scan found too little: {exported:?}"
    );
    let missing: Vec<_> = exported.difference(&header).collect();
    let stale: Vec<_> = header.difference(&exported).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "shim.h is out of sync with ovffi.\n  exported but not declared: {missing:?}\n  declared but not exported: {stale:?}"
    );
}

#[test]
fn video_packet_layout_matches() {
    let header = strip_comments(HEADER);
    let c_fields = fields_after(&header, "typedef struct ov_video_packet {", |l| {
        let l = l.strip_suffix(';')?;
        Some(ident_before(l, l.len()).to_owned())
    });
    let rust_fields = fields_after(SOURCE, "pub struct OvVideoPacket {", |l| {
        let l = l.strip_prefix("pub ")?;
        Some(l.split(':').next()?.trim().to_owned())
    });
    assert_eq!(
        c_fields, rust_fields,
        "ov_video_packet / OvVideoPacket field order differs"
    );
}

#[test]
fn shared_constants_match() {
    assert_eq!(
        header_define("OV_VIDEO_KEYFRAME"),
        i64::from(crate::OV_VIDEO_KEYFRAME)
    );
    assert_eq!(
        header_define("OV_VIDEO_END"),
        i64::from(crate::OV_VIDEO_END)
    );
    assert_eq!(
        header_define("OV_ERR_PANIC"),
        i64::from(crate::OV_ERR_PANIC)
    );
    for (name, value) in [
        ("OV_ERR_AUTH_FAILED", crate::OV_ERR_AUTH_FAILED),
        ("OV_ERR_RATE_LIMITED", crate::OV_ERR_RATE_LIMITED),
        (
            "OV_ERR_INCOMPATIBLE_VERSION",
            crate::OV_ERR_INCOMPATIBLE_VERSION,
        ),
        ("OV_ERR_HANDSHAKE", crate::OV_ERR_HANDSHAKE),
    ] {
        assert_eq!(header_define(name), i64::from(value), "{name}");
    }
}
