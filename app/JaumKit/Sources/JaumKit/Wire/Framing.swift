import Foundation

/// Length-prefixed framing shared with the daemon: 4-byte big-endian payload
/// length followed by the JSON payload.
public enum WireFraming {
    public enum EncodeError: Error {
        case payloadTooLarge(count: Int)
    }

    public static func encode(_ message: some Encodable) throws -> Data {
        let payload = try JSONEncoder().encode(message)
        // Mirror the Rust side (u32::try_from at protocol.rs): a payload past
        // 4 GiB is an error, not a silent overflow.
        guard let length = UInt32(exactly: payload.count) else {
            throw EncodeError.payloadTooLarge(count: payload.count)
        }
        var data = Data(count: 4)
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
    public enum FramingError: Error {
        case oversizedFrame(length: UInt32)
    }

    /// Well above any real frame (a full 200x60 screen is under 2 MiB); a
    /// corrupted length prefix fails fast instead of buffering forever.
    public static let maxFrameLength: UInt32 = 16 * 1024 * 1024

    private var buffer = Data()

    public init() {}

    public mutating func feed<T: Decodable>(_ chunk: Data, as type: T.Type) throws -> [T] {
        buffer.append(chunk)
        var messages: [T] = []
        while let payload = try popPayload() {
            messages.append(try JSONDecoder().decode(type, from: payload))
        }
        return messages
    }

    private mutating func popPayload() throws -> Data? {
        guard buffer.count >= 4 else { return nil }
        let length =
            (UInt32(buffer[buffer.startIndex]) << 24)
            | (UInt32(buffer[buffer.startIndex + 1]) << 16)
            | (UInt32(buffer[buffer.startIndex + 2]) << 8)
            | UInt32(buffer[buffer.startIndex + 3])
        guard length <= Self.maxFrameLength else {
            throw FramingError.oversizedFrame(length: length)
        }
        let total = 4 + Int(length)
        guard buffer.count >= total else { return nil }
        let payload = buffer.subdata(
            in: (buffer.startIndex + 4)..<(buffer.startIndex + total))
        buffer.removeSubrange(buffer.startIndex..<(buffer.startIndex + total))
        return payload
    }
}
