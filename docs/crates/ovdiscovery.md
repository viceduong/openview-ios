# `ovdiscovery` — LAN discovery (mDNS/DNS-SD), STUN, UPnP IGD, NAT inference

> Source: `crates/ovdiscovery/`
> Dependencies: `ovtransport` (declared; nothing in `src/` uses it), `thiserror`, `socket2` (picks the multicast interface for SSDP). Pure Rust over `std::net`.

## 1. Overview

This crate holds four self-contained helpers, all hand-written over `std::net`:

| Area | Module | Used by the shipped binaries? |
|---|---|---|
| mDNS/DNS-SD advertise and browse | `mdns.rs` + `dns_wire.rs` | browse only: `ovview::lan_find` (`crates/ovview/src/main.rs`). **No binary starts an `Advertiser`** (grep across `crates/`). |
| STUN Binding client (RFC 5389) | `stun.rs` | yes: `ovhost` direct-path candidate gathering (`crates/ovhost/src/direct_net.rs`; `--stun`, default `stun.cloudflare.com:3478`) |
| UPnP IGD port mapping | `upnp.rs` | yes: `ovhost` maps its direct UDP port (default 4570) with a 1 h lease, refreshed at half-life and deleted on exit |
| Symmetric-NAT inference | `nat.rs` | tests only (`crates/ovtransport/tests/punch_loopback.rs`) |

The direct-path engine that uses the STUN and UPnP candidates is described in [ovtransport.md §8](ovtransport.md).

## 2. Module map and public API

| Module | Public items |
|---|---|
| `lib.rs` | re-exports `encode_name`, `decode_name`, `encode_response`, `Record`, `DiscoveryError`, `browse`, `browse_loopback`, `Advertiser`, `DiscoveredPeer`, `MDNS_GROUP`, `MDNS_PORT`, `SERVICE_TCP`, `SERVICE_UDP`, `detect_symmetric`; `pub mod stun`, `pub mod upnp` |
| `error.rs` | `DiscoveryError { BadName(String), Truncated, Io(io::Error), Advertise(String) }`. Used by mDNS and DNS only; STUN and UPnP return `io::Result`. |
| `dns_wire.rs` | `encode_name(&str)`, `decode_name(packet, start) -> (String, consumed)`, `Record::{Ptr, Srv, Txt, A}`, `encode_response(&[Record])` |
| `mdns.rs` | `Advertiser::start(bind, fingerprint, udp_port, tcp_port, pairing_code)`, `.local_addr()`, `.stop()` (also runs on Drop); `browse(bind, deadline)`; `browse_loopback(bind, responders, deadline)`; `DiscoveredPeer { fingerprint: [u8;16], udp_port, tcp_port: Option<u16> }` |
| `stun.rs` | `DEFAULT_STUN`, `binding_request(&txid) -> [u8;20]`, `parse_binding_response(d, &txid) -> Option<SocketAddr>`, `query(&UdpSocket, server, timeout) -> io::Result<SocketAddr>` |
| `upnp.rs` | `Gateway { addr, control_path, service, local_ip }` with `.external_ip()`, `.add_udp(ext, int, lease, desc)`, `.delete_udp(ext)`; `PortMapping::create(port, lease, timeout)`, `.refresh()`, `.remove()`, deleted on Drop; helpers `msearch`, `parse_location`, `split_url`, `parse_control_url`, `soap_body`, `dechunk`, `local_ip_toward`, `discover(timeout)` |
| `nat.rs` | `detect_symmetric(probe_a_external, probe_b_reported) -> bool` (true when the two addresses differ) |

## 3. mDNS / DNS-SD (`mdns.rs`, `dns_wire.rs`)

### 3.1 Why hand-written

The module doc explains: the `mdns-sd` crate joins OS multicast groups on real interfaces and spawns its own threads, which makes deterministic loopback-only tests impossible. The responder here can bind to `127.0.0.1`, and the tests send it queries by unicast.

### 3.2 Services and records

| Item | Value |
|---|---|
| Group / port | `224.0.0.251:5353` (`MDNS_GROUP`, `MDNS_PORT`) |
| UDP service | `_openview._udp.local`, instance `openview-<first 4 fingerprint bytes, hex>._openview._udp.local` |
| TCP (pairing-code) service | `_openview._tcp.local`, instance `ov-<6-digit pairing code>._openview._tcp.local`. Only advertised when `tcp_port` is `Some` |
| TXT | `id=<32 lowercase hex chars>`, the 16-byte identity fingerprint, on both instances |
| SRV | priority 0, weight 0, port = UDP P2P port or TCP port, target `openview.local` |
| A | `openview.local` → **always `127.0.0.1`**, hard-coded |
| TTL / class | TTL 0; class IN with the cache-flush bit off |

**Response packet** (`encode_response`):

- Header: ID 0, flags `0x8400` (response, authoritative), QD 0, AN = number of records, NS 0, AR 0.
- Owner names are compressed against a table of suffixes already written to the packet. RDATA names use a separate throwaway table, because offsets inside RDATA are not packet offsets.

**Query packet** (`build_query`): ID 0, flags 0, one question, `QTYPE` PTR (12), `QCLASS` IN (1).

**`decode_name`** follows compression pointers. It rejects forward or self pointers (each pointer must point strictly backwards from the last one), more than 32 jumps, and the reserved label types `0b01` / `0b10`. Encoding rejects labels longer than 63 bytes and skips empty labels.

### 3.3 Advertiser

`Advertiser::start` binds `bind` with a 100 ms read timeout. For a non-loopback address it joins `224.0.0.251` on `INADDR_ANY` (best effort). It then runs one thread:

1. Wait for a packet whose **first question name** equals `_openview._udp.local` or `_openview._tcp.local` (case-insensitive).
2. Reply by unicast to the sender with all records: PTR, SRV, TXT, A, plus the TCP set when configured.

`stop()` and Drop set a flag and join the thread. The thread checks the flag after each 100 ms receive timeout.

### 3.4 Browse

- **`browse(bind, deadline)`**
  - For a **loopback** `bind` it returns an empty list immediately; use `browse_loopback` instead.
  - Otherwise it joins the group and repeats until the deadline: send both PTR queries to `224.0.0.251:5353`, then collect answers for 200 ms. It returns as soon as at least one peer collapses out of the answers.
- **`browse_loopback(bind, responders, deadline)`** sends unicast queries to known responder sockets in 50 ms rounds. It returns once a round adds no new answers and at least one peer is known.
- **`collapse`** maps instance name → TXT `id`, keeps only SRV records whose instance has an `id`, and merges the UDP and TCP ports by fingerprint. A peer without the UDP service is dropped. Output order is the order of first appearance.

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
| `SERVICE_UDP` / `SERVICE_TCP` | `_openview._udp.local` / `_openview._tcp.local` | `mdns.rs` |
| Advertiser read tick | 100 ms | `mdns.rs` |
| Browse query cycle / loopback round | 200 ms / 50 ms | `mdns.rs` |
| `DEFAULT_STUN` | `stun.cloudflare.com:3478` | `stun.rs` |
| STUN retransmit interval | 300 ms | `stun.rs` |
| `SSDP_ADDR` | 239.255.255.250:1900 | `upnp.rs` |
| M-SEARCH repeat / HTTP timeout | 700 ms / 3 s | `upnp.rs` |
| mapping description | `"OpenView direct"` | `upnp.rs` |

## 8. Error handling

- **mDNS / DNS:** `DiscoveryError`. In `browse` and `browse_loopback`, `parse_answers(...)?` turns **one malformed packet into an error for the whole browse call**. On a real LAN, any device that sends an odd mDNS packet to the group can abort a browse.
- **STUN and UPnP:** `io::Result`. A timeout is `TimedOut` or `NotFound`. UPnP faults are `io::Error::other` with the UPnP error code.
- **Advertiser:** multicast join errors are ignored (no LAN visibility, but no failure). Receive errors and timeouts are treated as ticks.

## 9. Tests

| File | Tests |
|---|---|
| `src/dns_wire.rs` | `name_roundtrip`, `empty_and_bad_names`, `decode_follows_compression_pointers`, `response_contains_txt_and_srv` |
| `src/mdns.rs` | `txt_id_roundtrip`, `query_matches_service_names`, `collapse_dedupes_and_merges_services`, `collapse_ignores_srv_without_id` |
| `src/stun.rs` | `request_layout`; RFC 5769 IPv4 and IPv6 sample responses (including a wrong transaction ID); `query_against_local_server` (fake XOR-MAPPED reply); `#[ignore] live_stun_query` (public server) |
| `src/upnp.rs` | `ssdp_location_parse`, `device_description_control_url`, `soap_envelope_shape`, `tag_with_attributes`, `chunked_body`, `soap_call_against_fake_router` (AddPortMapping + GetExternalIPAddress against a fake control point; checks the SOAPAction, internal client, and lease); `#[ignore] live_igd_readonly` |
| `src/nat.rs` | same mapping = cone; different port, IP, or address family = symmetric |
| `tests/discovery_loopback.rs` | advertise, then browse finds the peer with both ports; browse against a dead responder returns empty without hanging; two advertisers are deduplicated and told apart (one with no TCP service) |

## 10. Known limitations and TODOs found in code

1. **LAN discovery cannot work end to end in the shipped binaries.**
   - `DiscoveredPeer` carries **no IP address**: the browser discards the responder's source address.
   - The `A` record is hard-coded to `127.0.0.1`.
   - No shell ever starts an `Advertiser`.

   So `ovview`'s `lan_find` can only match peers that some other process advertises, and it still has to get the address from somewhere else.
2. **The production `Advertiser` is not bound to the mDNS port by its API.** It binds whatever `bind` it is given. To receive multicast queries it must be bound to `:5353`. Only unicast replies are sent (no multicast announcements), with TTL 0.
3. **One bad packet aborts a browse** (§8).
4. **`stun::query` leaves the socket non-blocking** and swallows unrelated datagrams while it runs.
5. **UPnP picks the interface by guessing.** It uses the route toward `8.8.8.8`, plus a unicast M-SEARCH to `x.y.z.1`, and handles IPv4 IGDs only.
6. **`detect_symmetric` is not wired** into connection setup. The crate description in `Cargo.toml` ("mDNS/DNS-SD … pairing-code instance names") predates the STUN and UPnP modules, and the `ovtransport` dependency is unused in `src/`.
