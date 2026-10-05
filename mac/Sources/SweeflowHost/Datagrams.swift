import Foundation

/// Video frames as QUIC datagrams. A frame message (as it would go on the window's stream) is
/// cut into pieces that each fit a datagram, with a 12-byte header:
/// u32 window, u32 frame sequence, u16 piece index, u16 piece count (little-endian).
enum Datagrams {
    /// Payload per datagram: the client's QUIC accepts ~1160 bytes per datagram on this path.
    static let chunk = 1100

    static func split(_ message: Data, wid: UInt32, seq: UInt32) -> [Data] {
        let count = max(1, (message.count + chunk - 1) / chunk)
        return (0..<count).map { i in
            var d = Data(capacity: 12 + chunk)
            withUnsafeBytes(of: wid.littleEndian) { d.append(contentsOf: $0) }
            withUnsafeBytes(of: seq.littleEndian) { d.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(i).littleEndian) { d.append(contentsOf: $0) }
            withUnsafeBytes(of: UInt16(count).littleEndian) { d.append(contentsOf: $0) }
            let start = message.startIndex + i * chunk
            d.append(message[start..<min(start + chunk, message.endIndex)])
            return d
        }
    }
}
