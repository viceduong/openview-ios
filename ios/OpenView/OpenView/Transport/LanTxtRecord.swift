import Foundation

// The TXT record an OpenView host publishes with its Bonjour service
// `<device-id>._openview._udp.local` (crates/ovdiscovery/src/mdns.rs):
//
//   id=<decimal device id>   v=<handshake protocol version>   name=<PC name>
//
// Pure value logic (no Network.framework) so the unit tests can compile it
// on its own.

struct LanTxtRecord: Equatable {
    /// The handshake protocol version this app speaks.
    static let protocolVersion = 3

    /// Device ID digits (`id=`), nil when absent or not a number.
    var deviceID: String?
    /// Handshake protocol version (`v=`).
    var version: Int?
    /// Display name (`name=`), trimmed; nil when empty.
    var name: String?

    init(deviceID: String? = nil, version: Int? = nil, name: String? = nil) {
        self.deviceID = deviceID
        self.version = version
        self.name = name
    }

    /// From key/value pairs (e.g. `NWTXTRecord.dictionary`). Keys are
    /// case-insensitive (RFC 6763 §6.4); when one appears twice in
    /// different case, either value is acceptable.
    init(dictionary: [String: String]) {
        self.init()
        for (key, value) in dictionary {
            apply(key: key, value: value)
        }
    }

    /// From raw TXT RDATA: a sequence of length-prefixed strings. The first
    /// occurrence of a key wins; a truncated final string is ignored.
    init(rdata: Data) {
        self.init()
        let bytes = [UInt8](rdata)
        var seen = Set<String>()
        var i = 0
        while i < bytes.count {
            let len = Int(bytes[i])
            i += 1
            guard i + len <= bytes.count else { break }
            let entry = bytes[i..<(i + len)]
            i += len
            guard let eq = entry.firstIndex(of: UInt8(ascii: "=")) else { continue }
            let key = String(decoding: entry[entry.startIndex..<eq], as: UTF8.self).lowercased()
            guard !key.isEmpty, !seen.contains(key) else { continue }
            seen.insert(key)
            let value = String(decoding: entry[(eq + 1)...], as: UTF8.self)
            apply(key: key, value: value)
        }
    }

    private mutating func apply(key: String, value: String) {
        let v = value.trimmingCharacters(in: .whitespacesAndNewlines)
        switch key.lowercased() {
        case "id":
            deviceID = LanTxtRecord.validDeviceID(v)
        case "v":
            version = Int(v)
        case "name":
            name = v.isEmpty ? nil : v
        default:
            break
        }
    }

    /// `text` when it is a usable device ID (digits only, fits a UInt64).
    static func validDeviceID(_ text: String) -> String? {
        guard !text.isEmpty, text.allSatisfy({ $0.isASCII && $0.isNumber }),
              UInt64(text) != nil else { return nil }
        return text
    }

    /// The host's device ID: TXT `id=`, else the Bonjour instance name
    /// when that is numeric (the host names its instance by device ID).
    func deviceID(instanceName: String) -> String? {
        deviceID ?? LanTxtRecord.validDeviceID(instanceName.trimmingCharacters(in: .whitespaces))
    }

    /// The name to show: TXT `name=`, else the grouped device ID.
    func displayName(instanceName: String) -> String {
        if let name { return name }
        if let id = deviceID(instanceName: instanceName) { return DeviceIDGrouping.format(id) }
        return instanceName
    }

    /// The host speaks our protocol version (or did not say).
    var isCompatible: Bool {
        version == nil || version == LanTxtRecord.protocolVersion
    }
}

/// "123456789" -> "123 456 789" (same grouping as `DeviceID.format`, kept
/// here so this file has no app dependencies).
enum DeviceIDGrouping {
    static func format(_ digits: String) -> String {
        var out = ""
        for (i, ch) in digits.enumerated() {
            if i > 0 && i % 3 == 0 {
                out.append(" ")
            }
            out.append(ch)
        }
        return out
    }
}
