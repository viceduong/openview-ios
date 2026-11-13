import XCTest

// The test target compiles RelayAddress.swift directly (no app host), so the
// types are used without `@testable import`.

final class RelayAddressTests: XCTestCase {
    func testWebSocketAddresses() {
        XCTAssertEqual(RelayAddress.parse("wss://relay.example.com/relay"),
                       .webSocket(URL(string: "wss://relay.example.com/relay")!))
        XCTAssertEqual(RelayAddress.parse("ws://10.0.0.2:8080/relay"),
                       .webSocket(URL(string: "ws://10.0.0.2:8080/relay")!))
        // The scheme is matched case-insensitively; whitespace is trimmed.
        XCTAssertEqual(RelayAddress.parse("  WSS://Relay.Example.com/relay \n"),
                       .webSocket(URL(string: "WSS://Relay.Example.com/relay")!))
    }

    func testTCPAddresses() {
        XCTAssertEqual(RelayAddress.parse("relay.example.com:4600"),
                       .tcp(host: "relay.example.com", port: 4600))
        XCTAssertEqual(RelayAddress.parse("192.168.1.5"),
                       .tcp(host: "192.168.1.5", port: RelayAddress.defaultTCPPort))
        XCTAssertEqual(RelayAddress.parse("[fe80::1]:4569"), .tcp(host: "fe80::1", port: 4569))
        XCTAssertEqual(RelayAddress.parse("[2001:db8::7]"), .tcp(host: "2001:db8::7", port: 4569))
        XCTAssertEqual(RelayAddress.parse("2001:db8::7"), .tcp(host: "2001:db8::7", port: 4569))
    }

    func testRejectsUnusableInput() {
        let bad = [
            "", "   ", "https://relay.example.com", "relay.example.com:0",
            "relay.example.com:99999", "relay.example.com:abc", ":4569",
            "relay example.com", "wss://", "relay.example.com/relay", "[fe80::1]:x", "[]:4569",
        ]
        for text in bad {
            XCTAssertNil(RelayAddress.parse(text), "accepted \(text.debugDescription)")
            XCTAssertFalse(RelayAddress.isValid(text))
        }
    }

    func testDefaultPortMatchesOvrelay() {
        XCTAssertEqual(RelayAddress.defaultTCPPort, 4569)
    }
}
