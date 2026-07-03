import Foundation

/// Length-prefixed framing shared with the daemon: 4-byte big-endian payload
/// length followed by the JSON payload.
public enum WireFraming {
    public static func encode(_ message: some Encodable) throws -> Data {
        let payload = try JSONEncoder().encode(message)
        var data = Data(count: 4)
        let length = UInt32(payload.count)
        data[0] = UInt8((length >> 24) & 0xFF)
        data[1] = UInt8((length >> 16) & 0xFF)
        data[2] = UInt8((length >> 8) & 0xFF)
        data[3] = UInt8(length & 0xFF)
        data.append(payload)
        return data
    }
}

/// Incremental decoder: feed raw socket bytes, pop complete messages.
/// Handles payloads split across reads and multiple messages per read.
public struct WireFrameDecoder: Sendable {
    private var buffer = Data()

    public init() {}

    public mutating func feed<T: Decodable>(_ chunk: Data, as type: T.Type) throws -> [T] {
        buffer.append(chunk)
        var messages: [T] = []
        while let payload = popPayload() {
            messages.append(try JSONDecoder().decode(type, from: payload))
        }
        return messages
    }

    private mutating func popPayload() -> Data? {
        guard buffer.count >= 4 else { return nil }
        let length =
            (UInt32(buffer[buffer.startIndex]) << 24)
            | (UInt32(buffer[buffer.startIndex + 1]) << 16)
            | (UInt32(buffer[buffer.startIndex + 2]) << 8)
            | UInt32(buffer[buffer.startIndex + 3])
        let total = 4 + Int(length)
        guard buffer.count >= total else { return nil }
        let payload = buffer.subdata(
            in: (buffer.startIndex + 4)..<(buffer.startIndex + total))
        buffer.removeSubrange(buffer.startIndex..<(buffer.startIndex + total))
        return payload
    }
}
