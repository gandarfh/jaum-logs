import Foundation

/// Bitflags serialized the way Rust's `bitflags` serde support does it:
/// flag names in declaration order joined by " | ", empty string for none.
public protocol WireFlagSet: OptionSet, Codable, Sendable where Element == Self {
    static var wireNames: [(Self, String)] { get }
}

extension WireFlagSet {
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        guard let flags = Self(wireString: raw) else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "unknown flag in: \(raw)"
            ))
        }
        self = flags
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(wireString)
    }

    public init?(wireString raw: String) {
        var flags = Self()
        let parts = raw.split(separator: "|").map { $0.trimmingCharacters(in: .whitespaces) }
        for part in parts where !part.isEmpty {
            guard let (flag, _) = Self.wireNames.first(where: { $0.1 == part }) else {
                return nil
            }
            flags.insert(flag)
        }
        self = flags
    }

    public var wireString: String {
        Self.wireNames
            .filter { contains($0.0) }
            .map(\.1)
            .joined(separator: " | ")
    }
}
