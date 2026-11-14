import Foundation

// The transport decision for one connection attempt: LAN first and/or the
// relay, and whether a failed LAN leg may fall back to the relay. Pure
// logic (Foundation only, no Network / OvFFI) so OpenViewTests compiles
// this file directly; SessionController feeds it the live network state
// (NetworkPathWatcher) and the attempt's failure.

/// One leg of a connection attempt.
enum DialRoute: Equatable {
    /// Direct UDP to the saved LAN address (`.direct`).
    case lan
    /// The relay (default relay URL), by device ID.
    case relay
}

/// What the phone's current network allows, as far as LAN dialing goes.
enum NetworkReach: Equatable {
    /// Wi-Fi or wired Ethernet is available: a LAN address may be reachable.
    case localNetwork
    /// Only cellular: a private LAN address cannot be reached.
    case cellularOnly
    /// No usable network at all.
    case offline
    /// Not known yet, or only other interfaces (e.g. a VPN tunnel without a
    /// visible Wi-Fi interface): try LAN first, the fallback covers a miss.
    case unknown

    /// Classify a network path. `satisfied`: the path can send;
    /// `hasLocalInterface`: Wi-Fi or wired Ethernet among its interfaces;
    /// `hasCellular`: a cellular interface among them.
    static func classify(satisfied: Bool?, hasLocalInterface: Bool, hasCellular: Bool) -> NetworkReach {
        guard let satisfied else { return .unknown }
        guard satisfied else { return .offline }
        if hasLocalInterface { return .localNetwork }
        if hasCellular { return .cellularOnly }
        return .unknown
    }
}

/// How an attempt leg ended, reduced to what the fallback decision needs.
enum LegFailure: Equatable {
    /// No answer from the host in time (LAN probe or handshake timeout).
    case timedOut
    /// The transport failed / the connection dropped before the session
    /// was established.
    case unreachable
    /// The host rejected the PIN or pairing code.
    case authFailed
    /// The host refuses PIN attempts for a while.
    case rateLimited
    /// The host speaks an incompatible protocol version.
    case incompatibleVersion
}

/// The ordered legs of one connection attempt.
struct TransportPlan: Equatable {
    /// How long a LAN leg may stay silent (no datagram from the host)
    /// before the relay is tried instead.
    static let lanProbeTimeout: TimeInterval = 3

    let routes: [DialRoute]

    /// Plan an attempt. `isLan`: the connection's transport is a LAN
    /// address; `hasDeviceID`: the target is a device ID, so the relay can
    /// reach the same host (pairing-code / ID-less targets stay LAN-only).
    ///
    /// - LAN + device ID on Wi-Fi / Ethernet (or unknown): LAN, then relay.
    /// - LAN + device ID on cellular / offline: relay directly.
    /// - LAN without a device ID: LAN only.
    /// - Relay transport: relay only.
    static func make(isLan: Bool, hasDeviceID: Bool, network: NetworkReach) -> TransportPlan {
        guard isLan else { return TransportPlan(routes: [.relay]) }
        guard hasDeviceID else { return TransportPlan(routes: [.lan]) }
        switch network {
        case .localNetwork, .unknown:
            return TransportPlan(routes: [.lan, .relay])
        case .cellularOnly, .offline:
            return TransportPlan(routes: [.relay])
        }
    }

    /// The first leg.
    var first: DialRoute {
        routes.first ?? .relay
    }

    /// The leg after `index`, if any.
    func route(after index: Int) -> DialRoute? {
        let next = index + 1
        return routes.indices.contains(next) ? routes[next] : nil
    }

    /// The leg at `index` has a later leg to fall back to.
    func hasFallback(after index: Int) -> Bool {
        route(after: index) != nil
    }

    /// Whether a leg that failed this way may move on to the next leg.
    /// Credential / lockout / version errors come from the host itself, so
    /// another route would get the same answer: surface them at once.
    static func shouldFallBack(after failure: LegFailure) -> Bool {
        switch failure {
        case .timedOut, .unreachable: return true
        case .authFailed, .rateLimited, .incompatibleVersion: return false
        }
    }

    /// The next leg to dial after the leg at `index` failed with `failure`
    /// (nil = no fallback: report the failure).
    func fallback(after index: Int, failure: LegFailure) -> DialRoute? {
        guard Self.shouldFallBack(after: failure) else { return nil }
        return route(after: index)
    }
}
