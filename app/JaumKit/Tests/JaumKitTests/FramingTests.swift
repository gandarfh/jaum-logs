import Foundation
import Testing

@testable import JaumKit

struct FramingTests {
    @Test func encodePrefixesBigEndianLength() throws {
        let framed = try WireFraming.encode(ClientMessage.resize(cols: 120, rows: 40))
        let payloadLength = framed.count - 4
        #expect(framed[0] == 0)
        #expect(framed[1] == 0)
        #expect(framed[2] == UInt8((payloadLength >> 8) & 0xFF))
        #expect(framed[3] == UInt8(payloadLength & 0xFF))
        let decoded = try JSONDecoder().decode(ClientMessage.self, from: framed.dropFirst(4))
        #expect(decoded == .resize(cols: 120, rows: 40))
    }

    @Test func decoderHandlesChunksSplitAnywhere() throws {
        var framed = try WireFraming.encode(ServerMessage.detach)
        framed.append(try WireFraming.encode(ServerMessage.runEditor(path: "/tmp/x.md")))

        for splitAt in 1..<framed.count {
            var decoder = WireFrameDecoder()
            let first = try decoder.feed(framed.prefix(splitAt), as: ServerMessage.self)
            let second = try decoder.feed(
                framed.suffix(framed.count - splitAt), as: ServerMessage.self)
            #expect(first + second == [.detach, .runEditor(path: "/tmp/x.md")])
        }
    }

    @Test func decoderReturnsAllMessagesInOneChunk() throws {
        var framed = Data()
        for message in [ClientMessage.editorDone, .shutdown, .resize(cols: 1, rows: 2)] {
            framed.append(try WireFraming.encode(message))
        }
        var decoder = WireFrameDecoder()
        let messages = try decoder.feed(framed, as: ClientMessage.self)
        #expect(messages == [.editorDone, .shutdown, .resize(cols: 1, rows: 2)])
    }

    @Test func decoderKeepsPartialPayloadBuffered() throws {
        let framed = try WireFraming.encode(ServerMessage.frameDiff([WireCell(x: 0, y: 0, sym: "a")]))
        var decoder = WireFrameDecoder()
        #expect(try decoder.feed(framed.prefix(2), as: ServerMessage.self).isEmpty)
        #expect(try decoder.feed(Data(), as: ServerMessage.self).isEmpty)
        let rest = try decoder.feed(framed.suffix(framed.count - 2), as: ServerMessage.self)
        #expect(rest == [.frameDiff([WireCell(x: 0, y: 0, sym: "a")])])
    }

    @Test func encodeAcceptsNormalPayloads() throws {
        let framed = try WireFraming.encode(ClientMessage.editorDone)
        #expect(framed.count > 4)
    }

    @Test func decoderRejectsOversizedFramesInsteadOfBuffering() {
        var prefix = Data()
        let huge = WireFrameDecoder.maxFrameLength + 1
        prefix.append(UInt8((huge >> 24) & 0xFF))
        prefix.append(UInt8((huge >> 16) & 0xFF))
        prefix.append(UInt8((huge >> 8) & 0xFF))
        prefix.append(UInt8(huge & 0xFF))
        var decoder = WireFrameDecoder()
        #expect(throws: WireFrameDecoder.FramingError.self) {
            try decoder.feed(prefix, as: ServerMessage.self)
        }
    }

    @Test func decoderThrowsOnMalformedPayload() throws {
        var framed = Data([0, 0, 0, 4])
        framed.append(Data("nope".utf8))
        var decoder = WireFrameDecoder()
        #expect(throws: (any Error).self) {
            try decoder.feed(framed, as: ServerMessage.self)
        }
    }
}
