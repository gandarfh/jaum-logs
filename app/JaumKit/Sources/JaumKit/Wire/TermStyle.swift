import Foundation

/// Mirrors ratatui's `Color` string serialization: named variants use the
/// canonical capitalized name, RGB uses uppercase `#RRGGBB`, and indexed
/// colors are bare decimal strings ("42").
public enum TermColor: Hashable, Sendable {
    case reset
    case named(NamedTermColor)
    case indexed(UInt8)
    case rgb(UInt8, UInt8, UInt8)
}

public enum NamedTermColor: String, CaseIterable, Sendable {
    case black = "Black"
    case red = "Red"
    case green = "Green"
    case yellow = "Yellow"
    case blue = "Blue"
    case magenta = "Magenta"
    case cyan = "Cyan"
    case gray = "Gray"
    case darkGray = "DarkGray"
    case lightRed = "LightRed"
    case lightGreen = "LightGreen"
    case lightYellow = "LightYellow"
    case lightBlue = "LightBlue"
    case lightMagenta = "LightMagenta"
    case lightCyan = "LightCyan"
    case white = "White"
}

extension TermColor: Codable {
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        guard let color = TermColor(wireString: raw) else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "unknown terminal color: \(raw)"
            ))
        }
        self = color
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(wireString)
    }

    public init?(wireString raw: String) {
        if raw == "Reset" {
            self = .reset
        } else if let named = NamedTermColor(rawValue: raw) {
            self = .named(named)
        } else if raw.hasPrefix("#"), raw.count == 7,
            let r = UInt8(raw.dropFirst().prefix(2), radix: 16),
            let g = UInt8(raw.dropFirst(3).prefix(2), radix: 16),
            let b = UInt8(raw.dropFirst(5).prefix(2), radix: 16)
        {
            self = .rgb(r, g, b)
        } else if let index = UInt8(raw) {
            self = .indexed(index)
        } else {
            return nil
        }
    }

    public var wireString: String {
        switch self {
        case .reset: "Reset"
        case .named(let named): named.rawValue
        case .indexed(let index): String(index)
        case .rgb(let r, let g, let b): String(format: "#%02X%02X%02X", r, g, b)
        }
    }
}

/// Mirrors ratatui's `Modifier` bitflags serialization.
public struct TermModifiers: WireFlagSet, Hashable {
    public let rawValue: UInt16

    public init(rawValue: UInt16) {
        self.rawValue = rawValue
    }

    public static let bold = TermModifiers(rawValue: 1 << 0)
    public static let dim = TermModifiers(rawValue: 1 << 1)
    public static let italic = TermModifiers(rawValue: 1 << 2)
    public static let underlined = TermModifiers(rawValue: 1 << 3)
    public static let slowBlink = TermModifiers(rawValue: 1 << 4)
    public static let rapidBlink = TermModifiers(rawValue: 1 << 5)
    public static let reversed = TermModifiers(rawValue: 1 << 6)
    public static let hidden = TermModifiers(rawValue: 1 << 7)
    public static let crossedOut = TermModifiers(rawValue: 1 << 8)

    public static let wireNames: [(TermModifiers, String)] = [
        (.bold, "BOLD"),
        (.dim, "DIM"),
        (.italic, "ITALIC"),
        (.underlined, "UNDERLINED"),
        (.slowBlink, "SLOW_BLINK"),
        (.rapidBlink, "RAPID_BLINK"),
        (.reversed, "REVERSED"),
        (.hidden, "HIDDEN"),
        (.crossedOut, "CROSSED_OUT"),
    ]
}
