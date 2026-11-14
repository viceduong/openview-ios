import XCTest

// The test target compiles TransportPlan.swift directly (no app host), so
// the types are used without `@testable import`.

/// `TransportPlan`: which legs a connection attempt dials (LAN first and /
/// or the relay) and when a failed LAN leg falls back to the relay.
final class TransportPlanTests: XCTestCase {
    func testLanWithDeviceIDOnWiFiTriesLanThenRelay() {
        let plan = TransportPlan.make(isLan: true, hasDeviceID: true, network: .localNetwork)
        XCTAssertEqual(plan.routes, [.lan, .relay])
        XCTAssertEqual(plan.first, .lan)
        XCTAssertTrue(plan.hasFallback(after: 0))
        XCTAssertEqual(plan.fallback(after: 0, failure: .timedOut), .relay)
        XCTAssertEqual(plan.fallback(after: 0, failure: .unreachable), .relay)
        // The relay is the last leg.
        XCTAssertFalse(plan.hasFallback(after: 1))
        XCTAssertNil(plan.fallback(after: 1, failure: .timedOut))
    }

    func testLanWithDeviceIDOnCellularGoesStraightToRelay() {
        let plan = TransportPlan.make(isLan: true, hasDeviceID: true, network: .cellularOnly)
        XCTAssertEqual(plan.routes, [.relay])
        XCTAssertEqual(plan.first, .relay)
        XCTAssertNil(plan.fallback(after: 0, failure: .timedOut))
        XCTAssertEqual(TransportPlan.make(isLan: true, hasDeviceID: true, network: .offline).routes,
                       [.relay])
    }

    func testUnknownNetworkStillTriesLanFirst() {
        XCTAssertEqual(TransportPlan.make(isLan: true, hasDeviceID: true, network: .unknown).routes,
                       [.lan, .relay])
    }

    func testAuthErrorsNeverFallBack() {
        let plan = TransportPlan.make(isLan: true, hasDeviceID: true, network: .localNetwork)
        for failure in [LegFailure.authFailed, .rateLimited, .incompatibleVersion] {
            XCTAssertFalse(TransportPlan.shouldFallBack(after: failure), "\(failure)")
            XCTAssertNil(plan.fallback(after: 0, failure: failure), "\(failure)")
        }
        XCTAssertTrue(TransportPlan.shouldFallBack(after: .timedOut))
        XCTAssertTrue(TransportPlan.shouldFallBack(after: .unreachable))
    }

    func testLanWithoutDeviceIDStaysLanOnly() {
        for network in [NetworkReach.localNetwork, .cellularOnly, .offline, .unknown] {
            let plan = TransportPlan.make(isLan: true, hasDeviceID: false, network: network)
            XCTAssertEqual(plan.routes, [.lan], "\(network)")
            XCTAssertNil(plan.fallback(after: 0, failure: .timedOut), "\(network)")
        }
    }

    func testRelayConnectionIsRelayOnly() {
        for network in [NetworkReach.localNetwork, .cellularOnly, .offline, .unknown] {
            XCTAssertEqual(TransportPlan.make(isLan: false, hasDeviceID: true, network: network).routes,
                           [.relay], "\(network)")
        }
    }

    func testNetworkClassification() {
        XCTAssertEqual(NetworkReach.classify(satisfied: nil, hasLocalInterface: true, hasCellular: false),
                       .unknown)
        XCTAssertEqual(NetworkReach.classify(satisfied: false, hasLocalInterface: true, hasCellular: true),
                       .offline)
        XCTAssertEqual(NetworkReach.classify(satisfied: true, hasLocalInterface: true, hasCellular: true),
                       .localNetwork)
        XCTAssertEqual(NetworkReach.classify(satisfied: true, hasLocalInterface: false, hasCellular: true),
                       .cellularOnly)
        // Only a tunnel / other interface: unknown, so LAN is still tried.
        XCTAssertEqual(NetworkReach.classify(satisfied: true, hasLocalInterface: false, hasCellular: false),
                       .unknown)
    }
}
