import Foundation

/// Minimal POSIX unix-socket server so the Network.framework transport is
/// exercised against a real socket, mirroring the daemon's accept loop.
final class UnixSocketServer: @unchecked Sendable {
    let path: String
    private let listenFD: Int32

    init(path: String) throws {
        self.path = path
        unlink(path)
        listenFD = socket(AF_UNIX, SOCK_STREAM, 0)
        guard listenFD >= 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8)
        precondition(pathBytes.count < MemoryLayout.size(ofValue: address.sun_path))
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            raw.copyBytes(from: pathBytes)
        }
        let size = socklen_t(MemoryLayout<sockaddr_un>.size)
        let bindResult = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPointer in
                bind(listenFD, sockaddrPointer, size)
            }
        }
        guard bindResult == 0, listen(listenFD, 1) == 0 else {
            close(listenFD)
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
    }

    /// Accepts one client on a background thread and hands it to `handler`,
    /// which owns the connection fd (it is closed afterwards).
    func acceptOnce(_ handler: @escaping @Sendable (Int32) -> Void) {
        let listenFD = self.listenFD
        DispatchQueue.global().async {
            let clientFD = accept(listenFD, nil, nil)
            guard clientFD >= 0 else { return }
            handler(clientFD)
            close(clientFD)
        }
    }

    func shutdown() {
        close(listenFD)
        unlink(path)
    }

    static func readExactly(_ count: Int, from fd: Int32) -> Data? {
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: count)
        while data.count < count {
            let n = read(fd, &buffer, count - data.count)
            guard n > 0 else { return nil }
            data.append(contentsOf: buffer[0..<n])
        }
        return data
    }

    static func readFrame(from fd: Int32) -> Data? {
        guard let prefix = readExactly(4, from: fd) else { return nil }
        let length =
            (UInt32(prefix[0]) << 24) | (UInt32(prefix[1]) << 16)
            | (UInt32(prefix[2]) << 8) | UInt32(prefix[3])
        return readExactly(Int(length), from: fd)
    }

    static func write(_ data: Data, to fd: Int32) {
        data.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let n = Foundation.write(fd, raw.baseAddress! + offset, raw.count - offset)
                guard n > 0 else { return }
                offset += n
            }
        }
    }
}
