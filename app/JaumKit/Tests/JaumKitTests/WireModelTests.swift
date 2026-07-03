import Foundation
import Testing

@testable import JaumKit

struct WireModelTests {
    private func encodeToString(_ value: some Encodable) throws -> String {
        String(decoding: try JSONEncoder().encode(value), as: UTF8.self)
    }

    @Test func termColorWireStrings() {
        #expect(TermColor.reset.wireString == "Reset")
        #expect(TermColor.named(.darkGray).wireString == "DarkGray")
        #expect(TermColor.indexed(7).wireString == "7")
        #expect(TermColor.rgb(180, 142, 255).wireString == "#B48EFF")
    }

    @Test func termColorParsesAllNamedVariants() {
        for named in NamedTermColor.allCases {
            #expect(TermColor(wireString: named.rawValue) == .named(named))
        }
        #expect(TermColor(wireString: "Reset") == .reset)
        #expect(TermColor(wireString: "#0A0b0C") == .rgb(10, 11, 12))
        #expect(TermColor(wireString: "255") == .indexed(255))
    }

    @Test func termColorRejectsGarbage() {
        #expect(TermColor(wireString: "chartreuse") == nil)
        #expect(TermColor(wireString: "#12345") == nil)
        #expect(TermColor(wireString: "999") == nil)
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(TermColor.self, from: Data("\"nope\"".utf8))
        }
    }

    @Test func termModifiersUseDeclarationOrder() throws {
        let mods: TermModifiers = [.underlined, .bold, .crossedOut]
        #expect(mods.wireString == "BOLD | UNDERLINED | CROSSED_OUT")
        #expect(TermModifiers(wireString: "BOLD | UNDERLINED | CROSSED_OUT") == mods)
        #expect(TermModifiers(wireString: "") == [])
        #expect(try encodeToString(TermModifiers([])) == "\"\"")
        #expect(TermModifiers(wireString: "GLOW") == nil)
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(TermModifiers.self, from: Data("\"GLOW\"".utf8))
        }
    }

    @Test func keyModifierNamesMatchCrossterm() {
        let all: KeyModifiers = [.shift, .control, .alt, .superKey, .hyper, .meta]
        #expect(all.wireString == "SHIFT | CONTROL | ALT | SUPER | HYPER | META")
        #expect(KeyEventState([.keypad, .capsLock, .numLock]).wireString == "KEYPAD | CAPS_LOCK | NUM_LOCK")
    }

    @Test func keyCodeUnitVariantsRoundtrip() throws {
        for (code, name) in KeyCode.unitNames {
            let encoded = try encodeToString(code)
            #expect(encoded == "\"\(name)\"")
            let decoded = try JSONDecoder().decode(KeyCode.self, from: Data(encoded.utf8))
            #expect(decoded == code)
        }
    }

    @Test func keyCodePayloadVariantsRoundtrip() throws {
        #expect(try encodeToString(KeyCode.f(5)) == "{\"F\":5}")
        #expect(try encodeToString(KeyCode.char("ç")) == "{\"Char\":\"ç\"}")
        let f = try JSONDecoder().decode(KeyCode.self, from: Data("{\"F\":12}".utf8))
        #expect(f == .f(12))
    }

    @Test func keyCodeRejectsUnknownShapes() {
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(KeyCode.self, from: Data("\"Hyperspace\"".utf8))
        }
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(KeyCode.self, from: Data("{\"Char\":\"ab\"}".utf8))
        }
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(KeyCode.self, from: Data("{\"Nope\":1}".utf8))
        }
    }

    @Test func mouseEventKindRoundtrip() throws {
        #expect(try encodeToString(MouseEventKind.down(.left)) == "{\"Down\":\"Left\"}")
        #expect(try encodeToString(MouseEventKind.up(.right)) == "{\"Up\":\"Right\"}")
        #expect(try encodeToString(MouseEventKind.drag(.middle)) == "{\"Drag\":\"Middle\"}")
        for (kind, name) in MouseEventKind.unitNames {
            #expect(try encodeToString(kind) == "\"\(name)\"")
            let decoded = try JSONDecoder().decode(
                MouseEventKind.self, from: Data("\"\(name)\"".utf8))
            #expect(decoded == kind)
        }
        let up = try JSONDecoder().decode(MouseEventKind.self, from: Data("{\"Up\":\"Left\"}".utf8))
        #expect(up == .up(.left))
        let drag = try JSONDecoder().decode(
            MouseEventKind.self, from: Data("{\"Drag\":\"Left\"}".utf8))
        #expect(drag == .drag(.left))
    }

    @Test func mouseEventKindRejectsUnknownShapes() {
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(MouseEventKind.self, from: Data("\"Warp\"".utf8))
        }
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(MouseEventKind.self, from: Data("{\"Nope\":\"Left\"}".utf8))
        }
    }

    @Test func clientMessageRejectsUnknownShapes() {
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(ClientMessage.self, from: Data("\"Reboot\"".utf8))
        }
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(ClientMessage.self, from: Data("{\"Nope\":1}".utf8))
        }
    }

    @Test func serverMessageRejectsUnknownShapes() {
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(ServerMessage.self, from: Data("\"Reboot\"".utf8))
        }
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(ServerMessage.self, from: Data("{\"Nope\":1}".utf8))
        }
    }
}
