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
typedef void (*ov_clipboard_sink)(void *user, uint8_t kind, const uint8_t *text, size_t len);
typedef void (*ov_file_sink)(void *user, const char *name, const uint8_t *data, size_t len);

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
int32_t ov_screen_set_clipboard_sink(OvScreen *screen, void *user, ov_clipboard_sink sink);
int32_t ov_screen_set_file_sink(OvScreen *screen, void *user, ov_file_sink sink);
int32_t ov_screen_feed(OvScreen *screen, const uint8_t *data, size_t len);

// liveness (v0.16): keepalive pacing + RTT + dead-peer detection
int32_t ov_screen_set_liveness_sink(OvScreen *screen, void *user, ov_liveness_sink sink);
int64_t ov_screen_keepalive_tick(OvScreen *screen, int64_t now_ms);
int32_t ov_screen_liveness_tick(OvScreen *screen, int64_t now_ms);
int32_t ov_screen_take_pong_reply(OvScreen *screen, uint8_t *out, size_t cap);

#endif
