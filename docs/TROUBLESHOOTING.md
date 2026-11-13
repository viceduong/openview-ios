# Troubleshooting

Start with the logs. In `OpenView.exe`: **Settings > Troubleshooting > Open
logs folder** (`%LOCALAPPDATA%\OpenView\logs`, or `data\logs` next to the
exe in portable mode):

| File | Contents |
|---|---|
| `openview.log` | The app itself, including host status changes |
| `ovhost.log`, `ovrelay.log` | stderr of the host and relay the app runs |
| `ovview-<id>.log`, `ovview-<id>.status.jsonl` | A viewer session: its log and its JSON status events |
| `panics.log` | Crash reports |

When running the binaries yourself, logs go to stderr: add `-v` (debug) or
`-vv` (trace), or set a filter with `--log-level` / `OPENVIEW_LOG`
(for example `info,ovtransport=debug`). Machine-readable status events are
printed on stdout as JSON lines (schema in
[crates/ovcommon.md](crates/ovcommon.md)).

Before filing an issue, check [KNOWN_ISSUES.md](KNOWN_ISSUES.md). Remove
PINs, device IDs and addresses you want to keep private from anything you
post.

## Common problems

| Symptom | Cause / fix |
|---|---|
| Windows SmartScreen blocks `OpenView.exe` or the installer | The binaries are not code-signed yet. Verify the file against `SHA256SUMS`, then **More info > Run anyway**. |
| The phone or another PC cannot reach the relay on this PC | Allow `ovrelay.exe` (TCP 4569) in the firewall and make sure the network is *Private*; see [INSTALL.md § Firewall](INSTALL.md#firewall). |
| `bad relay ...` right after pressing **Connect** | The relay address is not an IP, `hostname[:port]` or `ws://` / `wss://` URL (for example `192.168.1.20`, `relay.example.com:4569`, `wss://relay.example.com/relay`). |
| `WebSocket upgrade rejected: ...` | The `ws://` / `wss://` URL does not lead to `ovrelay` (wrong path or host, or the proxy does not forward WebSocket upgrades). |
| `TLS connection to ... failed` | The `wss://` server's certificate is not trusted by Windows, or a proxy intercepts TLS. |
| Sessions work but stay on the relay | UDP 4570 is blocked or the NAT does not allow a direct path. Allow `ovhost.exe` in the firewall, enable UPnP on the router, or accept the relay path. |
| Nothing happens when clicking in an elevated window (Task Manager, installers, UAC) | The host runs as a normal user and Windows blocks input into elevated windows and the secure desktop (UIPI). See [SECURITY.md](../SECURITY.md#known-limitations). |
| The device ID changed | `ovhost-unattended.id` was deleted, or the data folder changed (portable mode on/off, `--data-dir`, `OPENVIEW_DATA_DIR`). |

## Messages

Each entry gives the message as printed by the code, followed by its cause
or fix.

**Connecting**

| Message | Cause / fix |
|---|---|
| `ovview: host did not attach within 35s` / `host not reachable (relay park timed out with no host)` | No host is parked under that code or ID on this relay. Check that both sides use the **same relay address**, that the host shows `waiting on relay ...`, and the code or device ID. Remember that a relay started with an explicit `--bind` must use the port the clients dial (default 4569). |
| `relay <addr>: connect failed: ...` (either side), or in the GUI "Can't reach the relay at ... (connection refused)" | The relay is not running or the TCP port is blocked |
| `bad relay `x`: ...` | `--relay` must be an IP, `hostname[:port]` or a `ws://` / `wss://` URL |
| `handshake failed: authentication failed ...`, or in the GUI "A device tried to connect with a wrong PIN." | Wrong PIN or code, or a device-ID mismatch |
| `handshake timed out`, or in the GUI "A device started to connect but the connection did not complete" | The viewer's packets stopped arriving mid-handshake (Wi-Fi loss, firewall, the viewer gave up). LAN sessions retransmit lost handshake packets; update both sides if it persists |
| `handshake timed out` | The peer attached but the handshake did not finish in time (about 2 s on the viewer, 10 s on the host). |
| `--unattended needs a PIN (OPENVIEW_PIN or --pin)` / `--connect needs the host's PIN (OPENVIEW_PIN or --pin)` | Set `OPENVIEW_PIN` (or pass `--pin`) |
| `pass exactly one of --connect, --code or --id` (or a clap usage error) | Viewer target flags are missing or combined |
| The GUI host shows "Not ready" right after a restart | The relay still held the old park slot ("session already has a responder"). The GUI restarts its own relay for this reason; otherwise wait about 30 s. |

**Screen and input**

| Message | Cause / fix |
|---|---|
| `DXGI capture unavailable (...); using GDI` | RDP session, rotated output, secure desktop, or the region is not inside one output. GDI works but is slower; DXGI is retried every 2 s. |
| `DXGI delivers no desktop image (no active monitor / display off?); using GDI until it does` | The display is off or the output is headless. Capture switches back to DXGI on the first presented frame. |
| `input injection refused (blocked desktop / UIPI?)` | `SendInput` was refused. The code suggests a blocked desktop or UIPI: input to an elevated window from a non-elevated host, or to the secure desktop. |
| `Ctrl+Alt+Del: SendSAS unavailable (...)` | See [USAGE.md §2.5](USAGE.md#25-unattended-pin-host-always-reachable) (session 0 + `SoftwareSASGeneration`) |
| `released N stuck key(s)/button(s) after 30s without input` | Keys or buttons held by a viewer that vanished were released. This is expected. |
| `H.264 encoder failed (...); staying on lossless tiles` / `ovview: no H.264 decoder (...)` | The Media Foundation H.264 encoder or decoder could not be opened. The session continues on tiles. Use `--h264 sw` (or the hidden `--no-gpu-encode`) to try other encoder paths. |
| `Media Foundation is not installed (Windows N/KN without the Media Feature Pack); H.264 off, streaming lossless tiles` | Expected on Windows N/KN; install the Media Feature Pack to get H.264. Both binaries still start (`mfplat.dll` is delay-loaded). |
| `link-limited: ~N fps (a frame drains in N ms)` | Bandwidth sets the frame rate. Lower the resolution (`--screen WxH`) or use F1 to choose a fixed rate. |
| `MMCSS registration failed` | The capture thread keeps normal priority. Harmless. |

**Audio**

| Message | Cause / fix |
|---|---|
| `mic unavailable, audio disabled: ...` / `system audio unavailable, ...` | The audio device could not be opened (for example, there is none). The session continues without audio. |
| No sound in the viewer | Playback starts off: press **F2** or pass `--audio`. Check that **M** (mute) was not toggled by typing. |

**Direct path**

| Message | Cause / fix |
|---|---|
| `direct path disabled (UDP 4570: ...)` | The port is in use (another host instance?). Use `--direct-port`. |
| `LAN sessions disabled (UDP 4571: ...)` | The LAN port is in use (another host instance?). Use `--lan-port` (or `0` for any free port; mDNS announces it). |
| `device N was not found on this network` (`ovview --id`) | No mDNS answer: the host is not unattended, runs with `--no-mdns` / `--no-lan`, is on another subnet, or multicast / UDP 5353 is blocked. Dial its address with `--lan-addr IP:4571`, use `--connect` through the relay, or add `--relay-fallback`. |
| "Computers on this network" stays empty | Same causes as above; the Windows network must be *Private* and `ovhost.exe` allowed in the firewall (UDP 4571 and 5353). Hosts older than this release do not announce themselves. |
| `direct: STUN via ... failed` / `direct: UPnP mapping unavailable` | No public candidate. The direct path may still work on a LAN; otherwise the session stays on the relay. |

**Desktop effects and the GUI**

| Message | Cause / fix |
|---|---|
| Wallpaper stays hidden after a crash | Start `ovhost` once. It logs `restored desktop effects left over by a previous run`, using the marker `ovhost-desktopfx.state`. Effects are never written to the profile, so signing out also restores them. |
| GUI: "ovhost.exe is missing. It must be in the same folder as OpenView.exe ..." | Reinstall, or put the binaries next to `OpenView.exe` (`OPENVIEW_BIN_DIR` for development) |
| GUI: "No relay configured ..." | Enter **Settings → Relay → Relay address**, or turn on **Run a relay on this PC** |
| GUI: "Can't stop the other host (it may run as administrator)" | End `ovhost.exe` in Task Manager, then use **Stop it and take over** again |

**Liveness**: the viewer logs `peer disconnected (no inbound for 15s)` when
the peer goes quiet, and ends the session after 30 s of silence. The host
pauses capture after 15 s and ends the session after 30 s (`peer silent for
30s; ending session`).
