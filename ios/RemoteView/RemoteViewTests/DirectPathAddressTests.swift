import XCTest

/// `DirectPathSet.parse` / `classify`: the candidate addresses the host
/// offers ("ip:port" / "[v6]:port") and the LAN / Internet label.
final class DirectPathAddressTests: XCTestCase {
    func testParsesIPv4AndIPv6Candidates() {
        let v4 = DirectPathSet.parse("192.168.1.20:4570")
        XCTAssertEqual(v4?.host, "192.168.1.20")
        XCTAssertEqual(v4?.port, 4570)

        let v6 = DirectPathSet.parse("[fe80::1%en0]:4570")
        XCTAssertEqual(v6?.host, "fe80::1%en0")
        XCTAssertEqual(v6?.port, 4570)

        let spaced = DirectPathSet.parse(" 203.0.113.7:1 ")
        XCTAssertEqual(spaced?.host, "203.0.113.7")
        XCTAssertEqual(spaced?.port, 1)
    }

    func testRejectsMalformedCandidates() {
        for text in ["", "1.2.3.4", "1.2.3.4:0", "1.2.3.4:70000", ":4570",
                     "fe80::1:4570", "[fe80::1]", "[]:4570", "[fe80::1]:"] {
            XCTAssertNil(DirectPathSet.parse(text), "accepted \(text.debugDescription)")
        }
    }

    func testClassifiesPrivateRangesAsLAN() {
        for host in ["10.1.2.3", "127.0.0.1", "192.168.0.9", "169.254.3.4",
                     "172.16.0.1", "172.31.255.1", "100.64.0.1", "100.127.1.1",
                     "::1", "fe80::1", "fd12::3"] {
            XCTAssertEqual(DirectPathSet.classify(host: host), .lan, host)
        }
    }

    func testClassifiesPublicAddressesAsInternet() {
        for host in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "203.0.113.7",
                     "2001:db8::1", "1.2.3"] {
            XCTAssertEqual(DirectPathSet.classify(host: host), .internet, host)
        }
    }
}
