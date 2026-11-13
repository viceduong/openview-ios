# Security policy

OpenView gives remote control of a computer to whoever authenticates, so we
take security reports seriously and appreciate responsible disclosure.

## Supported versions

| Version | Supported |
|---|---|
| Latest release (`0.1.x`) | Yes |
| Older releases, development builds | No: please upgrade first |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately through GitHub:
[**Report a vulnerability**](https://github.com/OWNER/openview/security/advisories/new)
(repository *Security* tab > *Advisories* > *Report a vulnerability*).

Include what you found, the affected component (host, viewer, relay, Windows
app, iOS app, protocol), the version, and steps or a proof of concept. Do
not include real PINs, device IDs or addresses of machines you do not own.

What to expect:

- an acknowledgement within 7 days;
- an assessment and, for confirmed issues, a fix or mitigation plan; we
  aim to release fixes for serious issues within 90 days;
- credit in the advisory and the changelog, unless you prefer otherwise.

Please give us a reasonable time to ship a fix before disclosing publicly,
and only test against machines and relays you own or are allowed to test.

## Threat model (summary)

**Protected**

- **Session content.** Everything between viewer and host (screen, input,
  audio, clipboard, files) is end-to-end encrypted with ChaCha20-Poly1305
  under keys from an authenticated X25519 handshake (ephemeral and static
  keys, Ed25519 signatures, transcript hash). The relay and the network
  cannot read or modify it.
- **PIN access.** Unattended access is authenticated with SPAKE2 using the
  PIN, with key confirmation in both directions. An eavesdropper or a
  malicious relay learns nothing that allows offline guessing; each
  connection attempt tests exactly one PIN. The host throttles failures
  (5 per source address, 10 globally, then a lockout starting at 30 s and
  doubling up to 15 minutes), which limits online guessing to roughly a
  hundred attempts per day once saturated.
- **PIN handling on Windows.** `OpenView.exe` stores PINs in Windows
  Credential Manager and passes them to `ovhost` / `ovview` in the
  `OPENVIEW_PIN` environment variable, never on a command line.
- **Received files** are saved under `Downloads\OpenView` with sanitized
  names and never overwrite existing files; parsers of network input are
  bounded and fuzzed (`fuzz/`).

**Not protected / out of scope**

- A compromised host or viewer machine, or someone who knows the PIN.
- Other software running as the same Windows user (it can read that
  user's Credential Manager entries and OpenView's data folder).
- Denial of service against a relay you expose to the internet (the relay
  has connection caps but no authentication).

## Known limitations

- **Pairing-code mode is not a PAKE.** The one-time 6-digit code of
  attended sessions (`ovhost` without a PIN, `ovview --code`) is derived
  from the host's public fingerprint and sent in the handshake; it is a
  rendezvous and authorization value, not a secret. Use PIN mode for
  anything exposed beyond a trusted network, and check the fingerprint for
  attended sessions.
- **The host runs as the signed-in user.** It is not a Windows service:
  it cannot see or control the lock screen, the sign-in screen, UAC
  prompts or other elevated windows (Windows blocks that input), and a real
  Ctrl+Alt+Del is only possible with extra setup. Signing out stops it.
- **The relay sees metadata, not content**: the IP addresses of both sides,
  the session ID (derived from the device ID or pairing code), connection
  times and traffic volume. Run your own relay and put it behind TLS
  (`wss://`) if that matters to you.
- **Relays are open.** Anyone who can reach your relay can park and dial
  sessions through it (they still need the PIN to reach a host).
- **No replay window in the record layer** (SEC-6 in
  [KNOWN_ISSUES.md](docs/KNOWN_ISSUES.md)): correctness relies on the
  transport delivering records exactly once and in order.
- **Classical cryptography only**: the key exchange is X25519 with no
  post-quantum hybrid.
- **Unsigned binaries.** Release binaries are not code-signed yet; verify
  downloads against `SHA256SUMS` from the release page.

The full list of open issues is in
[docs/KNOWN_ISSUES.md](docs/KNOWN_ISSUES.md); the protocol is specified in
[docs/WIRE_PROTOCOL.md](docs/WIRE_PROTOCOL.md) and
[docs/crates/ovcore.md](docs/crates/ovcore.md).

## Hardening tips

- Use a long, random PIN (the app generates one; **Change** lets you set
  your own).
- Expose a relay to the internet only through TLS (`wss://` behind a
  reverse proxy or tunnel) or a VPN, and keep it updated.
- Allow the inbound firewall rules on private networks only (the
  installer's rules do exactly that).
- Turn off **Start OpenView at sign-in** or pause remote access when you do
  not need it.
