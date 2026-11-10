// OpenView C FFI shim — the surface ovffi exposes, declared for Swift via
// the OvFFI module (see module.modulemap). Keep in sync with
// openview/crates/ovffi/src/lib.rs.
#ifndef OV_SHIM_H
#define OV_SHIM_H

#include <stdint.h>
#include <stddef.h>

typedef void (*ov_datagram_sink)(void *user, const uint8_t *data, size_t len);
typedef void (*ov_record_sink)(void *user, uint8_t channel, const uint8_t *data, size_t len);
typedef void (*ov_state_sink)(void *user, int32_t state, const uint8_t *fp, size_t fp_len);
typedef void (*ov_frame_sink)(void *user, uint64_t seq, uint32_t width, uint32_t height,
                              uint32_t stride, uint8_t format, const uint8_t *pixels, size_t len);
typedef void (*ov_cursor_sink)(void *user, int32_t x, int32_t y);
typedef void (*ov_cursor_shape_sink)(void *user, uint16_t hotspot_x, uint16_t hotspot_y,
                                     uint16_t w, uint16_t h, const uint8_t *bgra, size_t len);
typedef void (*ov_liveness_sink)(void *user, int32_t alive, int32_t rtt_ms, float fps);
typedef void (*ov_monitors_sink)(void *user, const uint8_t *mons, size_t count);
typedef void (*ov_audio_sink)(void *user, const uint8_t *samples, size_t len);
// Phase 4: one Opus packet (48 kHz mono, normally 20 ms) passed through
// undecoded; `samples` = decoded sample count (960 for a 20 ms tick).
typedef void (*ov_opus_sink)(void *user, const uint8_t *packet, size_t len, uint32_t samples);
// Phase 5: one H.264 access unit for VideoToolbox (layout = ovffi
// `OvVideoPacket`, #[repr(C)]). The picture (w x h, even) goes at (x, y) of
// the screen_w x screen_h framebuffer the frame sink delivers. `data` is
// AVCC: every NAL unit prefixed by its 4-byte big-endian length, parameter
// sets / AUDs removed (CMSampleBuffer-ready, nalUnitHeaderLength 4). On a
// keyframe (flags & OV_VIDEO_KEYFRAME) `sps` / `pps` are the raw parameter
// set NAL units for CMVideoFormatDescriptionCreateFromH264ParameterSets;
// otherwise NULL / 0. flags & OV_VIDEO_END: motion over, no picture — the
// next frame-sink delivery is the lossless refresh; drop the overlay then.
// pts_us = host capture time (us since the UNIX epoch). Every pointer is
// valid only during the callback.
#define OV_VIDEO_KEYFRAME 1u
#define OV_VIDEO_END 2u
typedef struct ov_video_packet {
    uint64_t seq;
    uint64_t pts_us;
    uint32_t flags;
    uint32_t x;
    uint32_t y;
    uint32_t w;
    uint32_t h;
    uint32_t screen_w;
    uint32_t screen_h;
    uint32_t reserved;
    const uint8_t *sps;
    size_t sps_len;
    const uint8_t *pps;
    size_t pps_len;
    const uint8_t *data;
    size_t data_len;
    // Section 4 (additive): the stream's colour description as H.264 /
    // ISO 23001-8 code points from the SPS VUI (host default: BT.709
    // limited range = 1 / 1 / 1 / 0). Four u8s after a size_t: the struct
    // stays 8-byte aligned (4 bytes of tail padding), same as the Rust
    // #[repr(C)] OvVideoPacket.
    uint8_t colour_primaries;
    uint8_t transfer;
    uint8_t matrix;
    uint8_t full_range;
} ov_video_packet;
typedef void (*ov_video_sink)(void *user, const ov_video_packet *packet);
typedef void (*ov_clipboard_sink)(void *user, uint8_t kind, const uint8_t *text, size_t len);
typedef void (*ov_file_sink)(void *user, const char *name, const uint8_t *data, size_t len);
// Section 5 direct path: put `data` on the wire of path `path_id` (the UDP
// NWConnection the shell opened for it). Fires on the pump / send caller
// thread, under the session lock: never call back into the session.
typedef void (*OvDirectSendSink)(void *user, uint32_t path_id, const uint8_t *data, size_t len);
// Section 5: the host offered candidate `addr` ("ip:port" or "[v6]:port",
// NUL-terminated, valid only during the call); open a UDP path for it.
typedef void (*OvDirectPathSink)(void *user, uint32_t path_id, const char *addr);
// Wave 1 event-driven relay receive: the Rust-owned TCP relay socket has
// inbound bytes — dispatch an ov_session_pump_relay onto the session queue.
// Fires on a Rust watcher thread (NOT the pump thread): only dispatch,
// never call into the session synchronously, never block. Coalesced: the
// next wake comes only after a pump drained the socket.
typedef void (*ov_wake_sink)(void *user);
// Wave 1 zero-copy frames: `pixels` = the assembler's framebuffer (`len`
// bytes, `height` rows of `stride` bytes, format 0 = BGRA / 1 = BGR),
// `rects` = `rect_count` packed u16 quadruples x, y, w, h the last frame
// changed (rect_count 0 = the whole frame). Valid only during the call.
typedef void (*ov_framebuffer_visitor)(void *user, const uint8_t *pixels, size_t len,
                                       uint32_t width, uint32_t height, uint32_t stride,
                                       uint8_t format, uint64_t seq,
                                       const uint16_t *rects, size_t rect_count);

// Opaque handle types: Swift imports `void *` as UnsafeMutableRawPointer
// but the app stores OpaquePointer, so declare the handles as pointers to
// incomplete structs (zero ABI change — a pointer is a pointer).
typedef struct OvIdentity OvIdentity;
typedef struct OvSession OvSession;
typedef struct OvScreen OvScreen;

// identity
OvIdentity *ov_identity_create(void);
void ov_identity_free(OvIdentity *id);
int32_t ov_identity_fingerprint(const OvIdentity *id, uint8_t *out /*16*/);
int32_t ov_identity_fingerprint_pine(const OvIdentity *id, char *out, size_t cap);
int32_t ov_identity_pairing_code(const OvIdentity *id, char *out, size_t cap);

// session
OvSession *ov_session_create(int32_t is_server, const char *pairing_code);
void ov_session_free(OvSession *s);
int32_t ov_session_set_datagram_sink(OvSession *s, void *user, ov_datagram_sink sink);
int32_t ov_session_on_datagram(OvSession *s, const uint8_t *data, size_t len);
int32_t ov_session_begin(OvSession *s, int32_t auth_mode);
// v0.22 unattended access: begin the handshake in PIN mode. `pin` is the
// access password (pin_len bytes) — the host's PIN on the client side, the
// PIN clients must present on the server side.
int32_t ov_session_begin_pin(OvSession *s, const uint8_t *pin, size_t pin_len);
int32_t ov_session_pump(OvSession *s);
int32_t ov_session_send_record(OvSession *s, uint8_t channel, const uint8_t *data, size_t len);
int32_t ov_session_set_record_sink(OvSession *s, void *user, ov_record_sink sink);
int32_t ov_session_set_state_sink(OvSession *s, void *user, ov_state_sink sink);
int32_t ov_session_peer_fingerprint(OvSession *s, uint8_t *out /*16*/);
int32_t ov_session_relay_connect(OvSession *s, const char *relay_host, uint16_t relay_port, uint32_t session_id);
int32_t ov_session_pump_relay(OvSession *s);
// relay over a host-owned byte stream (wss://host/relay): outbound chunks go
// to `sink` (send each as one binary message); inbound messages via _feed.
int32_t ov_session_relay_connect_stream(OvSession *s, void *user, ov_datagram_sink sink, uint32_t session_id);
int32_t ov_session_relay_feed(OvSession *s, const uint8_t *data, size_t len);
int32_t ov_session_relay_eof(OvSession *s);
int32_t ov_relay_session_id(const char *code, uint8_t *out /*4*/);
// Wave 1: wake the shell as soon as the TCP relay socket has bytes (the
// watcher starts with ov_session_relay_connect, before or after this call).
// Returns 1 = socket watched, 0 = stored but nothing to watch (no relay yet /
// WebSocket bridge), -1 null session, -4 watcher thread failed. NULL sink =
// stop watching. Keep a slow timer pump as the fallback.
int32_t ov_session_set_wake_sink(OvSession *s, void *user, ov_wake_sink sink);

// Section 5: direct (UDP) path next to a relay session. Installing the sinks
// makes the session direct-capable (pair with ov_screen_set_direct_capable);
// probing / promotion / fallback run inside the pump calls.
int32_t ov_session_set_direct_sinks(OvSession *s, void *user, OvDirectSendSink send, OvDirectPathSink open_path);
// Queue one datagram received on direct path `path_id` (processed by the
// next pump). 0, -1 bad args, -2 no direct engine (not established yet).
int32_t ov_session_on_direct_datagram(OvSession *s, uint32_t path_id, const uint8_t *data, size_t len);
int32_t ov_session_direct_state(OvSession *s);        /* 0 relay, 1 probing, 2 direct, -1 null */
int64_t ov_session_direct_deadline_ms(OvSession *s);  /* -1 = no timer pending */
int64_t ov_session_direct_path(OvSession *s);         /* active path_id or -1 */
// Section 4: SESSION_OPTION payload (key 1 = hide wallpaper, 2 = disable
// animations; value 0/1) for ov_session_send_record on channel 0. Returns
// the length (3) or -1.
int32_t ov_session_option_record(uint8_t key, uint8_t value, uint8_t *out, size_t cap);

// file transfer (v0.27): send a named file to the peer in 16 KiB chunks.
int32_t ov_file_send(OvSession *s, const char *name, const uint8_t *data, size_t data_len);

// screen frames (terminal-channel reassembly, kept in Rust)
OvScreen *ov_screen_new(void);
void ov_screen_free(OvScreen *screen);
int32_t ov_screen_set_frame_sink(OvScreen *screen, void *user, ov_frame_sink sink);
int32_t ov_screen_set_cursor_sink(OvScreen *screen, void *user, ov_cursor_sink sink);
int32_t ov_screen_set_cursor_shape_sink(OvScreen *screen, void *user, ov_cursor_shape_sink sink);
int32_t ov_screen_set_monitors_sink(OvScreen *screen, void *user, ov_monitors_sink sink);
int32_t ov_screen_set_audio_sink(OvScreen *screen, void *user, ov_audio_sink sink);
// Phase 4: installing an Opus sink (before the first ov_screen_feed) makes
// our CAPS advertise CAP_OPUS; the host then sends Opus packets here
// instead of raw PCM to the audio sink.
int32_t ov_screen_set_opus_sink(OvScreen *screen, void *user, ov_opus_sink sink);
// Phase 5: installing a video sink (before the first ov_screen_feed, and
// only when VideoToolbox can decode H.264) makes our CAPS advertise
// CAP_H264; the host then streams high-motion regions as H.264 packets.
int32_t ov_screen_set_video_sink(OvScreen *screen, void *user, ov_video_sink sink);
// Phase 5: the shell's decoder failed — drop P-frames until the next IDR and
// queue a VIDEO_IDR_REQUEST (sent via ov_screen_take_pong_reply). Never call
// it from inside a sink (ov_screen_feed is still running): defer it.
int32_t ov_screen_request_video_idr(OvScreen *screen);
int32_t ov_screen_set_clipboard_sink(OvScreen *screen, void *user, ov_clipboard_sink sink);
int32_t ov_screen_set_file_sink(OvScreen *screen, void *user, ov_file_sink sink);
int32_t ov_screen_feed(OvScreen *screen, const uint8_t *data, size_t len);
// Section 2: the host's CAPS bits (0 until the host answered; an old host
// never does). CAP_INPUT2 = 0x40 enables POINTER_EX / WHEEL / KEY_EX / TEXT /
// RELEASE_ALL / SAS. Session queue only (OvScreen is not locked).
uint32_t ov_screen_peer_caps(const OvScreen *screen);
// Section 5: advertise CAP_DIRECT (1 << 7) in our CAPS (before the first
// screen command, alongside ov_session_set_direct_sinks).
int32_t ov_screen_set_direct_capable(OvScreen *s, int32_t on);
// Phase 3: the rects the last delivered frame changed, as packed u16
// quadruples x, y, w, h (frame pixels) in `out` (room for max_rects).
// Returns the rect count (may exceed max_rects), 0 before any frame, -1 on
// a null handle. Session queue only, never from inside a sink.
int32_t ov_screen_last_dirty_rects(OvScreen *s, uint16_t *out, size_t max_rects);
// Wave 1 zero-copy frames: call `visit` once, synchronously, with the
// framebuffer pointer + the last frame's dirty rects (full != 0: no rects,
// upload everything). Session queue only, right after the ov_screen_feed
// whose frame sink fired (or any time for a full refresh), never from
// inside a sink. 0 visited, -2 no framebuffer yet, -1 null handle/visitor.
int32_t ov_screen_visit_framebuffer(OvScreen *s, void *user, ov_framebuffer_visitor visit, int32_t full);

// liveness (v0.16): keepalive pacing + RTT + dead-peer detection
int32_t ov_screen_set_liveness_sink(OvScreen *screen, void *user, ov_liveness_sink sink);
int64_t ov_screen_keepalive_tick(OvScreen *screen, int64_t now_ms);
int32_t ov_screen_liveness_tick(OvScreen *screen, int64_t now_ms);
// Watchdog: ms since the last inbound record (same clock as liveness_tick).
int64_t ov_screen_inbound_age_ms(OvScreen *screen, int64_t now_ms);
int32_t ov_screen_take_pong_reply(OvScreen *screen, uint8_t *out, size_t cap);

#endif
