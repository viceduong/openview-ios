# `ovdiscovery` — LAN discovery (mDNS/DNS-SD), STUN, UPnP IGD, NAT inference

> Source: `crates/ovdiscovery/`
> Dependencies: `thiserror`, `socket2` (shared-port binds, per-interface multicast for mDNS and SSDP), `tracing`, and on Windows `windows` (`GetAdaptersAddresses`). Pure Rust over `std::net` otherwise.

## 1. Overview

| Area | Module | Used by |
|---|---|---|
| mDNS/DNS-SD advertise and browse, keyed by device ID | `mdns.rs` + `dns_wire.rs` | `ovhost` advertises (unattended mode), `ovview --id` resolves, `OpenView.exe` lists "Computers on this network" |
| LAN interface enumeration | `netif.rs` | the mDNS responder / browser; `ovhost` direct-path LAN candidates |
| STUN Binding client (RFC 5389) | `stun.rs` | `ovhost` direct-path candidate gathering (`crates/ovhost/src/direct_net.rs`; `--stun`, default `stun.cloudflare.com:3478`) |
| UPnP IGD port mapping | `upnp.rs` | `ovhost` maps its direct UDP port (default 4570) with a 1 h lease, refreshed at half-life and deleted on exit |
| Symmetric-NAT inference | `nat.rs` | tests only (`crates/ovtransport/tests/punch_loopback.rs`) |

The direct-path engine that uses the STUN and UPnP candidates is described in [ovtransport.md §8](ovtransport.md). The LAN session the mDNS records point at (a PIN handshake on the host's LAN port, no relay) is described in [ovhost.md](ovhost.md) and [USAGE.md §2.2](../USAGE.md).

## 2. Module map and public API

| Module | Public items |
|---|---|
| `lib.rs` | `pub mod mdns`, `pub mod netif`, `pub mod stun`, `pub mod upnp`; re-exports `AdvertiseConfig`, `Browser`, `DiscoveredHost`, `DiscoveryError`, `detect_symmetric` |
| `error.rs` | `DiscoveryError { BadName(String), Truncated, Io(io::Error), Advertise(String) }`. Used by mDNS and DNS only; STUN and UPnP return `io::Result`. |
| `mdns.rs` | `AdvertiseConfig { device_id, port, name, version, addrs, include_ipv6 }` + `::new(device_id, port)`; `Advertiser::start(cfg)`, `.instance_name()`, `.local_addr()`, `.stop()` (also on Drop; sends the goodbye); `Browser::start()`, `.hosts()`, `.find(id)`, `.stop()`; `browse(timeout)`, `resolve(device_id, timeout)`; `DiscoveredHost { device_id, name, addrs, version, last_seen }` + `.is_compatible()`; `instance_name(id)`, `default_display_name()`; constants `SERVICE_TYPE`, `MDNS_GROUP`, `MDNS_PORT`, `DEFAULT_LAN_PORT` (4571), `TXT_PROTOCOL_VERSION`; loopback test helpers `Advertiser::start_loopback`, `Browser::start_unicast` |
| `netif.rs` | `LocalInterface { name, index, v4: Vec<(Ipv4Addr, prefix)>, v6 }` + `.contains(ip)`; `lan_interfaces()`, `lan_ipv4s()`, `same_subnet`, `usable_v4`, `usable_v6` |
| `dns_wire.rs` (private) | `Message { id, flags, questions, answers, authorities, additionals }` with `encode()` (name compression) / `parse()` (bounds-checked); `Question`, `ResourceRecord`, `RData::{Ptr, Srv, Txt, A, Aaaa, Other}`, `txt_pairs` |
| `stun.rs` | `DEFAULT_STUN`, `binding_request(&txid) -> [u8;20]`, `parse_binding_response(d, &txid) -> Option<SocketAddr>`, `query(&UdpSocket, server, timeout) -> io::Result<SocketAddr>` |
| `upnp.rs` | `Gateway { addr, control_path, service, local_ip }` with `.external_ip()`, `.add_udp(ext, int, lease, desc)`, `.delete_udp(ext)`; `PortMapping::create(port, lease, timeout)`, `.refresh()`, `.remove()`, deleted on Drop; helpers `msearch`, `parse_location`, `split_url`, `parse_control_url`, `soap_body`, `dechunk`, `local_ip_toward`, `discover(timeout)` |
| `nat.rs` | `detect_symmetric(probe_a_external, probe_b_reported) -> bool` (true when the two addresses differ) |

## 3. mDNS / DNS-SD (`mdns.rs`, `dns_wire.rs`, `netif.rs`)

A hand-written RFC 6762 / 6763 responder and browser over `std::net` + `socket2`. It needs no system service (Bonjour / Avahi) and shares UDP 5353 with whatever mDNS stack the OS runs.

### 3.1 Records

An unattended host publishes the instance `<device-id>._openview._udp.local`. The decimal device ID is the instance label, so the name is unique per machine and needs no conflict probing.

| Record | Owner | Data | TTL |
|---|---|---|---|
| PTR | `_openview._udp.local` | the instance | 4500 s (shared) |
| PTR | `_services._dns-sd._udp.local` | `_openview._udp.local` | 4500 s (shared) |
| SRV | instance | port = the host's **LAN session port** (`ovhost --lan-port`, default 4571), target `openview-<id>.local` | 120 s, cache-flush |
| TXT | instance | `id=<device id>`, `v=<handshake protocol version, 2>`, `name=<computer name>` | 4500 s, cache-flush |
| A / AAAA | `openview-<id>.local` | every LAN address of the host (AAAA only with `include_ipv6`) | 120 s, cache-flush |

The host name is `openview-<id>.local`, not the computer name, so it never contradicts the Windows responder that owns `<COMPUTERNAME>.local`. The display name in TXT is truncated to fit one 255-byte TXT string.

**Addresses** come from `netif::lan_interfaces()`:

- **Windows:** `GetAdaptersAddresses` (AF_UNSPEC). Adapters must be up. Skipped: loopback, tunnel and `IF_TYPE_PROP_VIRTUAL` adapters (Tailscale, WireGuard), adapters flagged "no multicast", and Hyper-V / WSL / VirtualBox / VMware adapters **without a gateway** (host-only switches; an external Hyper-V switch carries the real LAN address and has one). Adapters with a gateway come first.
- **Elsewhere:** the address of the default route (no prefix known; `/24` assumed).
- IPv4 `169.254/16`, loopback and unspecified addresses are dropped; IPv6 keeps global (`2000::/3`) and unique-local (`fc00::/7`) addresses.

### 3.2 Responder (`Advertiser`)

- One UDP socket on `0.0.0.0:5353` with `SO_REUSEADDR` (and `SO_REUSEPORT` on Unix), IP TTL 255, multicast loop on, joined to `224.0.0.251` on every interface address. Binding fails only if another process holds 5353 exclusively.
- The interface list is re-read every 30 s: new interfaces are joined and the records re-announced; addresses that disappeared get a goodbye.
- **Announcements:** the full record set twice, 1 s apart, on every interface. **Goodbye:** the same records with TTL 0 on `stop()` / Drop. `ovhost` registers the stop with its exit cleanup, so Ctrl-C also sends it.
- **Answers** (questions for the service type, the meta-service, the instance or the host name; ANY included):
  - source port ≠ 5353 → *legacy unicast* (§6.7): a unicast reply echoing the query ID and questions, TTL ≤ 10 s, no cache-flush bits;
  - QU bit set → a unicast reply to the querier;
  - otherwise a multicast reply on the interface(s) whose subnet contains the querier (all interfaces when none does); an identical multicast reply is sent at most once per second.
- Known answers the querier already holds with at least half their TTL are not repeated (§7.1).
- Not implemented: probing and conflict resolution (the name is unique by construction) and IPv6 multicast `ff02::fb` (every Bonjour / Avahi browser also listens on IPv4).

### 3.3 Browser

- `Browser::start()` runs one thread. It sends `PTR _openview._udp.local` from an **ephemeral port** out of every interface, so every responder answers by unicast and the replies arrive even when another process owns 5353. Query backoff: 0, 1, 2, 4, 8 s, then every 15 s.
- It also listens on a shared 5353 socket (best effort) for announcements and goodbyes.
- **Cache:** per instance (SRV, TXT, source address, expiry) and per host name (addresses). A/AAAA records are kept only for host names an SRV points at. Both maps are capped at 256 entries.
- **Expiry:** an entry lives for `max(TTL, 30 s)` after its last refresh. A goodbye (PTR or SRV with TTL 0) removes the instance at once.
- **Result:** a `DiscoveredHost` once SRV and at least one address are known. The device ID comes from TXT `id=` (else the numeric instance label), the name from TXT `name=` (else the label). Hosts are deduplicated by device ID and sorted by name.
- **Address order:** the source address of the responder's last packet first (it is on a link the browser can reach), then IPv4, then IPv6. Without A/AAAA records the source address is used.
- Malformed packets are logged at `debug` and skipped; they never end a browse.
- `browse(timeout)` runs a browser for `timeout` and returns the snapshot. `resolve(id, timeout)` returns as soon as that device is known, else `None`.

### 3.4 Wire codec (`dns_wire.rs`)

- `Message::encode` compresses names against every suffix already written (offsets below `0x4000`), including names inside PTR / SRV RDATA, which are written in place.
- `Message::parse` checks every length. Each section is capped at 1024 items. Names longer than 255 bytes, reserved label types, and pointers that do not move strictly backwards (max 32 jumps) are rejected. An RDATA name must stay inside its RDATA. Unknown record types are kept raw. Any error fails the whole message, and the caller skips it.
- `txt_pairs` splits TXT strings RFC 6763 style: keys are case-insensitive, the first occurrence wins, and a string without `=` is a boolean key.

## 4. STUN (`stun.rs`)

RFC 5389 Binding, client side only.

**Request** (20 bytes):

| Offset | Len | Field |
|---|---|---|
| 0 | 2 | type `0x0001` (Binding Request) |
| 2 | 2 | length `0` |
| 4 | 4 | magic cookie `0x2112A442` |
| 8 | 12 | transaction ID |

**Response parsing** (`parse_binding_response`):

- Requires type `0x0101` (Binding Success), the magic cookie, a matching transaction ID, and a body at least as long as the header length.
- Walks the attributes with 4-byte padding. `XOR-MAPPED-ADDRESS` (`0x0020`) wins. `MAPPED-ADDRESS` (`0x0001`) is the fallback.
- XOR decoding: the port is XORed with the cookie's top 16 bits. IPv4 is XORed with the cookie. IPv6 is XORed with cookie ‖ transaction ID.

**`query(sock, server, timeout)`**

- Resolves `server`, choosing the address family that matches the socket.
- Puts the socket in **non-blocking mode and leaves it that way**.
- Sends the request every 300 ms until the deadline and polls with 5 ms sleeps.
- Accepts only datagrams from the server address.
- Datagrams that are not the response are **consumed and dropped**, so call `query` before the socket carries session traffic (`ovhost` does this at startup).
- Times out with `io::ErrorKind::TimedOut`.

The transaction ID comes from `RandomState`-seeded hashing of the time, which is fine for a non-secret ID.

## 5. UPnP IGD (`upnp.rs`)

### 5.1 Discovery (`discover(timeout)`)

1. **Pick the LAN interface.** Find the local IPv4 toward `8.8.8.8` with the UDP-connect trick (`local_ip_toward`, which sends nothing). Bind the SSDP socket there and set `IP_MULTICAST_IF`. This avoids VPN and virtual adapters.
2. **Search.** Every 700 ms, send `M-SEARCH` for `InternetGatewayDevice:1` and `:2`:
   - to `239.255.255.250:1900`, and
   - by unicast to `x.y.z.1:1900`, a guessed gateway address.

   The M-SEARCH request is `M-SEARCH * HTTP/1.1`, `HOST`, `ST`, `MAN: "ssdp:discover"`, `MX: 2`.
3. **Resolve.** For each new `LOCATION` (from `200 OK` responses only), fetch the device description with HTTP/1.1 `GET` (3 s timeout).
4. **Find the control URL.** Look for the first matching service in this preference order: `WANIPConnection:2`, `WANIPConnection:1`, `WANPPPConnection:1`. Take the `controlURL` from the same `<service>` block. It may be absolute or relative.
5. **Result.** The `Gateway` records our LAN IPv4 toward the control host as `local_ip`. If nothing is found, the error is `NotFound` ("no UPnP IGD answered").

### 5.2 SOAP

Each call is a `POST <control_path>` with these headers: `Content-Type: text/xml; charset="utf-8"`, `SOAPAction: "<service>#<action>"`, `Connection: close`. The body is a standard SOAP 1.1 envelope (`soap_body`).

Response handling:

- The minimal HTTP client reads to EOF and handles `Transfer-Encoding: chunked` (`dechunk`).
- A status other than 200 becomes an error that carries the UPnP `errorCode` and `errorDescription`.
- Tag values are found by string search, which tolerates attributes on tags.

| Action | Arguments |
|---|---|
| `AddPortMapping` | `NewRemoteHost` = "", `NewExternalPort`, `NewProtocol` = UDP, `NewInternalPort`, `NewInternalClient` = local IP, `NewEnabled` = 1, `NewPortMappingDescription`, `NewLeaseDuration` (seconds) |
| `GetExternalIPAddress` | — (reads `NewExternalIPAddress`) |
| `DeletePortMapping` | `NewRemoteHost`, `NewExternalPort`, `NewProtocol` = UDP |

### 5.3 `PortMapping`

**`create(port, lease, timeout)`**

1. Discover the gateway.
2. Map UDP with the same external and internal port. If the router rejects a finite lease, retry with `0` (permanent).
3. Query the external IP.
4. The result has `external = ext_ip:port`.

**`refresh()`** re-adds the mapping once half the lease has elapsed. It does nothing for a permanent (`0`) lease. **`remove()`**, which also runs on Drop, deletes the mapping. The description is always `"OpenView direct"`.

## 6. NAT type (`nat.rs`)

`detect_symmetric(a, b)` returns `a != b`. If the relay saw two different mapped addresses for the same host's two probe sockets, the NAT is endpoint-dependent (symmetric), and the caller should stay on the relay. Nothing in `ovtransport::direct` or the shells calls it.

## 7. Configuration and constants

| Constant | Value | File |
|---|---|---|
| `MDNS_GROUP` / `MDNS_PORT` | 224.0.0.251 / 5353 | `mdns.rs` |
| `SERVICE_TYPE` | `_openview._udp.local` (Bonjour: `_openview._udp`) | `mdns.rs` |
| `TXT_PROTOCOL_VERSION` | 2 (= `ovcore::handshake::PROTOCOL_VERSION`) | `mdns.rs` |
| TTLs: SRV/A/AAAA, PTR/TXT, legacy unicast cap | 120 s, 4500 s, 10 s | `mdns.rs` |
| Announcements / interface refresh | 2 × 1 s apart / every 30 s | `mdns.rs` |
| Browser query backoff / minimum hold / cache cap | 0, 1, 2, 4, 8, then 15 s / 30 s / 256 | `mdns.rs` |
| `DEFAULT_STUN` | `stun.cloudflare.com:3478` | `stun.rs` |
| STUN retransmit interval | 300 ms | `stun.rs` |
| `SSDP_ADDR` | 239.255.255.250:1900 | `upnp.rs` |
| M-SEARCH repeat / HTTP timeout | 700 ms / 3 s | `upnp.rs` |
| mapping description | `"OpenView direct"` | `upnp.rs` |

## 8. Error handling

- **mDNS / DNS:** `DiscoveryError`. `Advertiser::start` fails only when UDP 5353 cannot be bound. `Browser::start` fails only when no UDP socket can be bound; a 5353 it cannot share just means announcements are not overheard. Malformed packets, receive errors (including Windows' `WSAECONNRESET` after an ICMP port-unreachable) and failed joins or sends are logged at `debug` and ignored.
- **STUN and UPnP:** `io::Result`. A timeout is `TimedOut` or `NotFound`. UPnP faults are `io::Error::other` with the UPnP error code.

## 9. Tests

| File | Tests |
|---|---|
| `src/dns_wire.rs` | name round trip and limits; full `Message` round trip with compression; empty TXT; unknown types kept raw; **every truncation of a valid packet fails cleanly**; 5000 random corruptions never panic; hostile counts and pointer loops; RDATA names that overrun their RDATA; `txt_pairs` rules |
| `src/mdns/tests.rs` | TXT contents and truncation; responder answers (PTR / instance / host / meta queries, case-insensitive, legacy-unicast format, known-answer suppression, responses ignored); announcement and goodbye; response → cache → `DiscoveredHost` round trip; address order; goodbye removal; expiry; foreign services ignored; ID fallback; cache cap; the browser skipping junk packets; a loopback advertise + browse |
| `src/netif.rs` | subnet matching, address filters, enumeration returns only usable addresses |
| `tests/lan_discovery.rs` | **real multicast**: advertise a random device ID on the machine's LAN interfaces, `resolve` it (a real LAN address, the right port and TXT), see it in a continuous `Browser`, and see the goodbye remove it; skips with a message when there is no LAN interface, 5353 cannot be shared, or multicast is filtered |
| `src/stun.rs` | `request_layout`; RFC 5769 IPv4 and IPv6 sample responses (including a wrong transaction ID); `query_against_local_server` (fake XOR-MAPPED reply); `#[ignore] live_stun_query` (public server) |
| `src/upnp.rs` | `ssdp_location_parse`, `device_description_control_url`, `soap_envelope_shape`, `tag_with_attributes`, `chunked_body`, `soap_call_against_fake_router` (AddPortMapping + GetExternalIPAddress against a fake control point; checks the SOAPAction, internal client, and lease); `#[ignore] live_igd_readonly` |
| `src/nat.rs` | same mapping = cone; different port, IP, or address family = symmetric |

End to end: `crates/ovhost` `lan_viewer_resolves_by_mdns_and_completes_the_pin_handshake` (advertise → resolve → PIN handshake over the LAN carrier, with an unreachable relay) and `scripts/e2e_lan.sh` (real `ovhost` + `ovview --id` processes).

## 10. Known limitations

1. **mDNS is unauthenticated.** Any device on the LAN can answer for a device ID. The PIN handshake still authenticates the host (`ovview` pins the device ID, and the iOS app pins a saved device's fingerprint), so a spoofed answer can only make the dial fail.
2. **IPv4 multicast only**; no probing or conflict resolution (see §3.2).
3. **Interface filtering is heuristic** (§3.1). A LAN without a default gateway that runs on a Hyper-V / VirtualBox / VMware adapter is skipped.
4. **`stun::query` leaves the socket non-blocking** and swallows unrelated datagrams while it runs.
5. **UPnP picks the interface by guessing.** It uses the route toward `8.8.8.8`, plus a unicast M-SEARCH to `x.y.z.1`, and handles IPv4 IGDs only.
6. **`detect_symmetric` is not wired** into connection setup yet.
