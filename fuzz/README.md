# Fuzzing

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets for the
parsers that see untrusted peer bytes. The crate is excluded from the main
workspace (it needs a nightly toolchain and libFuzzer).

| Target | What it feeds |
|---|---|
| `handshake_feed` | `ovcore::handshake::Handshake::feed`, both roles, every auth mode |
| `command_decode` | `ovcore::commands::Message::decode`, record header parsing |
| `bands_parse` | `ovapp::bands::parse_bounded` + `decode_bands` (screen band container) |
| `ws_frames` | `ovrelay` WebSocket deframer, tunnel-frame cursor, CONNECT line |
| `record_open` | `ovcore::session::Session::decrypt_record`: 16-byte header, anti-replay window, AEAD failure paths (`ovcore::fuzzing`) |
| `replay_window` | `ovcore::replay::ReplayWindow` against a model (never accepts a duplicate or too-old number) |

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run handshake_feed -- -max_total_time=300
```

libFuzzer is supported on Linux and macOS; on Windows use WSL. The same
entry points also run as deterministic smoke tests in the normal test suite
(`hostile_input_never_panics`, `hostile_containers_never_panic`,
`parser_entry_points_survive_noise`, `entry_points_survive_noise`), so
`cargo test` catches regressions without a fuzzer.

`ovrelay`'s and `ovcore`'s internals are reached through `ovrelay::fuzzing`
and `ovcore::fuzzing`, which only exist with each crate's `fuzzing` feature
(enabled by this crate).
