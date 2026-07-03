import Foundation

/// Mirrors crossterm's `KeyModifiers` bitflags serialization.
public struct KeyModifiers: WireFlagSet, Hashable {
    public let rawValue: UInt8

    public init(rawValue: UInt8) {
        self.rawValue = rawValue
    }

    public static let shift = KeyModifiers(rawValue: 1 << 0)
    public static let control = KeyModifiers(rawValue: 1 << 1)
    public static let alt = KeyModifiers(rawValue: 1 << 2)
    public static let superKey = KeyModifiers(rawValue: 1 << 3)
    public static let hyper = KeyModifiers(rawValue: 1 << 4)
    public static let meta = KeyModifiers(rawValue: 1 << 5)

    public static let wireNames: [(KeyModifiers, String)] = [
        (.shift, "SHIFT"),
        (.control, "CONTROL"),
        (.alt, "ALT"),
        (.superKey, "SUPER"),
        (.hyper, "HYPER"),
        (.meta, "META"),
    ]
}

/// Mirrors crossterm's `KeyEventState` bitflags serialization.
public struct KeyEventState: WireFlagSet, Hashable {
    public let rawValue: UInt8

    public init(rawValue: UInt8) {
        self.rawValue = rawValue
    }

    public static let keypad = KeyEventState(rawValue: 1 << 0)
    public static let capsLock = KeyEventState(rawValue: 1 << 1)
    public static let numLock = KeyEventState(rawValue: 1 << 2)

    public static let wireNames: [(KeyEventState, String)] = [
        (.keypad, "KEYPAD"),
        (.capsLock, "CAPS_LOCK"),
        (.numLock, "NUM_LOCK"),
    ]
}

public enum KeyEventKind: String, Codable, Sendable {
    case press = "Press"
    case `repeat` = "Repeat"
    case release = "Release"
}

/// Mirrors crossterm's `KeyCode` serde enum: unit variants are bare strings,
/// payload variants are single-key objects ({"Char": "p"}, {"F": 5}).
public enum KeyCode: Hashable, Sendable {
    case backspace
    case enter
    case left
    case right
    case up
    case down
    case home
    case end
    case pageUp
    case pageDown
    case tab
    case backTab
    case delete
    case insert
    case f(UInt8)
    case char(Character)
    case null
    case esc
    case capsLock
    case scrollLock
    case numLock
    case printScreen
    case pause
    case menu
    case keypadBegin
}

extension KeyCode: Codable {
    static let unitNames: [(KeyCode, String)] = [
        (.backspace, "Backspace"),
        (.enter, "Enter"),
        (.left, "Left"),
        (.right, "Right"),
        (.up, "Up"),
        (.down, "Down"),
        (.home, "Home"),
        (.end, "End"),
        (.pageUp, "PageUp"),
        (.pageDown, "PageDown"),
        (.tab, "Tab"),
        (.backTab, "BackTab"),
        (.delete, "Delete"),
        (.insert, "Insert"),
        (.null, "Null"),
        (.esc, "Esc"),
        (.capsLock, "CapsLock"),
        (.scrollLock, "ScrollLock"),
        (.numLock, "NumLock"),
        (.printScreen, "PrintScreen"),
        (.pause, "Pause"),
        (.menu, "Menu"),
        (.keypadBegin, "KeypadBegin"),
    ]

    private enum PayloadKeys: String, CodingKey {
        case char = "Char"
        case f = "F"
    }

    public init(from decoder: Decoder) throws {
        if let single = try? decoder.singleValueContainer(),
            let name = try? single.decode(String.self)
        {
            guard let (code, _) = Self.unitNames.first(where: { $0.1 == name }) else {
                throw DecodingError.dataCorrupted(DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "unknown key code: \(name)"
                ))
            }
            self = code
            return
        }
        let container = try decoder.container(keyedBy: PayloadKeys.self)
        if let text = try container.decodeIfPresent(String.self, forKey: .char) {
            guard text.count == 1, let char = text.first else {
                throw DecodingError.dataCorrupted(DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "Char payload must be a single character"
                ))
            }
            self = .char(char)
        } else if let number = try container.decodeIfPresent(UInt8.self, forKey: .f) {
            self = .f(number)
        } else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "unknown key code payload"
            ))
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .char(let char):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(String(char), forKey: .char)
        case .f(let number):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(number, forKey: .f)
        default:
            guard let (_, name) = Self.unitNames.first(where: { $0.0 == self }) else {
                throw EncodingError.invalidValue(self, EncodingError.Context(
                    codingPath: encoder.codingPath,
                    debugDescription: "key code without wire name"
                ))
            }
            var container = encoder.singleValueContainer()
            try container.encode(name)
        }
    }
}

/// Mirrors crossterm's `KeyEvent`.
public struct KeyEvent: Codable, Hashable, Sendable {
    public var code: KeyCode
    public var modifiers: KeyModifiers
    public var kind: KeyEventKind
    public var state: KeyEventState

    public init(
        code: KeyCode,
        modifiers: KeyModifiers = [],
        kind: KeyEventKind = .press,
        state: KeyEventState = []
    ) {
        self.code = code
        self.modifiers = modifiers
        self.kind = kind
        self.state = state
    }
}

public enum MouseButton: String, Codable, Sendable {
    case left = "Left"
    case right = "Right"
    case middle = "Middle"
}

/// Mirrors crossterm's `MouseEventKind` serde enum.
public enum MouseEventKind: Hashable, Sendable {
    case down(MouseButton)
    case up(MouseButton)
    case drag(MouseButton)
    case moved
    case scrollDown
    case scrollUp
    case scrollLeft
    case scrollRight
}

extension MouseEventKind: Codable {
    static let unitNames: [(MouseEventKind, String)] = [
        (.moved, "Moved"),
        (.scrollDown, "ScrollDown"),
        (.scrollUp, "ScrollUp"),
        (.scrollLeft, "ScrollLeft"),
        (.scrollRight, "ScrollRight"),
    ]

    private enum PayloadKeys: String, CodingKey {
        case down = "Down"
        case up = "Up"
        case drag = "Drag"
    }

    public init(from decoder: Decoder) throws {
        if let single = try? decoder.singleValueContainer(),
            let name = try? single.decode(String.self)
        {
            guard let (kind, _) = Self.unitNames.first(where: { $0.1 == name }) else {
                throw DecodingError.dataCorrupted(DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "unknown mouse event kind: \(name)"
                ))
            }
            self = kind
            return
        }
        let container = try decoder.container(keyedBy: PayloadKeys.self)
        if let button = try container.decodeIfPresent(MouseButton.self, forKey: .down) {
            self = .down(button)
        } else if let button = try container.decodeIfPresent(MouseButton.self, forKey: .up) {
            self = .up(button)
        } else if let button = try container.decodeIfPresent(MouseButton.self, forKey: .drag) {
            self = .drag(button)
        } else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "unknown mouse event payload"
            ))
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .down(let button):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(button, forKey: .down)
        case .up(let button):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(button, forKey: .up)
        case .drag(let button):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(button, forKey: .drag)
        default:
            guard let (_, name) = Self.unitNames.first(where: { $0.0 == self }) else {
                throw EncodingError.invalidValue(self, EncodingError.Context(
                    codingPath: encoder.codingPath,
                    debugDescription: "mouse event kind without wire name"
                ))
            }
            var container = encoder.singleValueContainer()
            try container.encode(name)
        }
    }
}

/// Mirrors crossterm's `MouseEvent`.
public struct MouseEvent: Codable, Hashable, Sendable {
    public var kind: MouseEventKind
    public var column: UInt16
    public var row: UInt16
    public var modifiers: KeyModifiers

    public init(kind: MouseEventKind, column: UInt16, row: UInt16, modifiers: KeyModifiers = []) {
        self.kind = kind
        self.column = column
        self.row = row
        self.modifiers = modifiers
    }
}
