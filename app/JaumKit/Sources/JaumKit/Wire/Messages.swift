import Foundation

/// A screen cell at position (x, y), exactly as the daemon serializes it.
public struct WireCell: Codable, Hashable, Sendable {
    public var x: UInt16
    public var y: UInt16
    public var sym: String
    public var fg: TermColor
    public var bg: TermColor
    public var underline: TermColor
    public var mods: TermModifiers

    public init(
        x: UInt16,
        y: UInt16,
        sym: String,
        fg: TermColor = .reset,
        bg: TermColor = .reset,
        underline: TermColor = .reset,
        mods: TermModifiers = []
    ) {
        self.x = x
        self.y = y
        self.sym = sym
        self.fg = fg
        self.bg = bg
        self.underline = underline
        self.mods = mods
    }
}

/// Client to daemon messages. Serde's externally tagged representation: unit
/// variants are bare strings, payload variants are single-key objects.
public enum ClientMessage: Hashable, Sendable {
    case key(KeyEvent)
    case mouse(MouseEvent)
    case resize(cols: UInt16, rows: UInt16)
    case editorDone
    case shutdown
}

extension ClientMessage: Codable {
    private enum PayloadKeys: String, CodingKey {
        case key = "Key"
        case mouse = "Mouse"
        case resize = "Resize"
    }

    private struct ResizePayload: Codable {
        var cols: UInt16
        var rows: UInt16
    }

    public init(from decoder: Decoder) throws {
        if let single = try? decoder.singleValueContainer(),
            let name = try? single.decode(String.self)
        {
            switch name {
            case "EditorDone": self = .editorDone
            case "Shutdown": self = .shutdown
            default:
                throw DecodingError.dataCorrupted(DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "unknown client message: \(name)"
                ))
            }
            return
        }
        let container = try decoder.container(keyedBy: PayloadKeys.self)
        if let event = try container.decodeIfPresent(KeyEvent.self, forKey: .key) {
            self = .key(event)
        } else if let event = try container.decodeIfPresent(MouseEvent.self, forKey: .mouse) {
            self = .mouse(event)
        } else if let size = try container.decodeIfPresent(ResizePayload.self, forKey: .resize) {
            self = .resize(cols: size.cols, rows: size.rows)
        } else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "unknown client message payload"
            ))
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .key(let event):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(event, forKey: .key)
        case .mouse(let event):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(event, forKey: .mouse)
        case .resize(let cols, let rows):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(ResizePayload(cols: cols, rows: rows), forKey: .resize)
        case .editorDone:
            var container = encoder.singleValueContainer()
            try container.encode("EditorDone")
        case .shutdown:
            var container = encoder.singleValueContainer()
            try container.encode("Shutdown")
        }
    }
}

/// Daemon to client messages.
public enum ServerMessage: Hashable, Sendable {
    case frameFull(cols: UInt16, rows: UInt16, cells: [WireCell])
    case frameDiff([WireCell])
    case detach
    case runEditor(path: String)
}

extension ServerMessage: Codable {
    private enum PayloadKeys: String, CodingKey {
        case frameFull = "FrameFull"
        case frameDiff = "FrameDiff"
        case runEditor = "RunEditor"
    }

    private struct FrameFullPayload: Codable {
        var cols: UInt16
        var rows: UInt16
        var cells: [WireCell]
    }

    private struct RunEditorPayload: Codable {
        var path: String
    }

    public init(from decoder: Decoder) throws {
        if let single = try? decoder.singleValueContainer(),
            let name = try? single.decode(String.self)
        {
            guard name == "Detach" else {
                throw DecodingError.dataCorrupted(DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "unknown server message: \(name)"
                ))
            }
            self = .detach
            return
        }
        let container = try decoder.container(keyedBy: PayloadKeys.self)
        if let frame = try container.decodeIfPresent(FrameFullPayload.self, forKey: .frameFull) {
            self = .frameFull(cols: frame.cols, rows: frame.rows, cells: frame.cells)
        } else if let cells = try container.decodeIfPresent([WireCell].self, forKey: .frameDiff) {
            self = .frameDiff(cells)
        } else if let editor = try container.decodeIfPresent(
            RunEditorPayload.self, forKey: .runEditor)
        {
            self = .runEditor(path: editor.path)
        } else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "unknown server message payload"
            ))
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .frameFull(let cols, let rows, let cells):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(
                FrameFullPayload(cols: cols, rows: rows, cells: cells), forKey: .frameFull)
        case .frameDiff(let cells):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(cells, forKey: .frameDiff)
        case .detach:
            var container = encoder.singleValueContainer()
            try container.encode("Detach")
        case .runEditor(let path):
            var container = encoder.container(keyedBy: PayloadKeys.self)
            try container.encode(RunEditorPayload(path: path), forKey: .runEditor)
        }
    }
}
