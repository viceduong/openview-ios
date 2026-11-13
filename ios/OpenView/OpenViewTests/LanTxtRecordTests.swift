import XCTest

/// `LanTxtRecord`: the TXT record an OpenView host publishes with its
/// Bonjour service (`id=`, `v=`, `name=`; crates/ovdiscovery/src/mdns.rs).
final class LanTxtRecordTests: XCTestCase {
    /// Raw TXT RDATA from length-prefixed strings.
    private func rdata(_ entries: [String]) -> Data {
        var d = Data()
        for e in entries {
            let bytes = Array(e.utf8)
            d.append(UInt8(bytes.count))
            d.append(contentsOf: bytes)
        }
        return d
    }

    func testParsesHostRecord() {
        let txt = LanTxtRecord(rdata: rdata(["id=123456789012", "v=3", "name=Office PC"]))
        XCTAssertEqual(txt.deviceID, "123456789012")
        XCTAssertEqual(txt.version, 3)
        XCTAssertEqual(txt.name, "Office PC")
        XCTAssertTrue(txt.isCompatible)
        XCTAssertEqual(txt.displayName(instanceName: "123456789012"), "Office PC")
    }

    func testDictionaryKeysAreCaseInsensitive() {
        let txt = LanTxtRecord(dictionary: ["ID": "42", "V": "4", "Name": "  Den  "])
        XCTAssertEqual(txt.deviceID, "42")
        XCTAssertEqual(txt.version, 4)
        XCTAssertFalse(txt.isCompatible)
        XCTAssertEqual(txt.name, "Den")
    }

    func testFirstKeyWinsAndJunkIsIgnored() {
        let txt = LanTxtRecord(rdata: rdata(["id=7", "flag", "=x", "id=8", "name=a=b"]))
        XCTAssertEqual(txt.deviceID, "7")
        XCTAssertEqual(txt.name, "a=b")
        XCTAssertNil(txt.version)
        XCTAssertTrue(txt.isCompatible, "no version = assume compatible")
    }

    func testRejectsBadDeviceIDs() {
        for bad in ["", "12a", "-5", "99999999999999999999", "１２３"] {
            XCTAssertNil(LanTxtRecord(dictionary: ["id": bad]).deviceID, bad)
        }
    }

    func testFallsBackToInstanceName() {
        let txt = LanTxtRecord(rdata: rdata(["v=2"]))
        XCTAssertEqual(txt.deviceID(instanceName: "123456789"), "123456789")
        XCTAssertEqual(txt.displayName(instanceName: "123456789"), "123 456 789")
        XCTAssertNil(txt.deviceID(instanceName: "Living Room"))
        XCTAssertEqual(txt.displayName(instanceName: "Living Room"), "Living Room")
    }

    func testTruncatedRdataKeepsCompleteEntries() {
        var d = rdata(["id=5", "name=PC"])
        d.append(contentsOf: [40, UInt8(ascii: "v")]) // claims 40 bytes, has 1
        let txt = LanTxtRecord(rdata: d)
        XCTAssertEqual(txt.deviceID, "5")
        XCTAssertEqual(txt.name, "PC")
        XCTAssertNil(txt.version)
        XCTAssertEqual(LanTxtRecord(rdata: Data()), LanTxtRecord())
        XCTAssertEqual(LanTxtRecord(rdata: Data([0])), LanTxtRecord())
    }
}
