import Foundation

/// Message type tags; see PROTOCOL.md.
enum Msg: UInt8 {
    case hello = 0x01, welcome = 0x02
    case windowAdded = 0x10, windowChanged = 0x11, windowRemoved = 0x12, appIcon = 0x13, cursor = 0x14, focused = 0x15
    case windowButtons = 0x16, moveStarted = 0x17, toggleMaximize = 0x18, minimize = 0x1A, menuTree = 0x1B, appList = 0x1C, screenLocked = 0x1D
    case resize = 0x20, focus = 0x21, close = 0x22, screenSize = 0x23, menuActivate = 0x24, menuRefresh = 0x25, restore = 0x26, launchApp = 0x27
    case pointerMove = 0x30, pointerButton = 0x31, scroll = 0x32, key = 0x33, releaseAll = 0x34
    case clipboard = 0x40, ping = 0x41, pong = 0x42, clipboardSubscribe = 0x43
    case dropFiles = 0x46, dragBegan = 0x47, dragTaken = 0x48, dragFetch = 0x49, dragData = 0x4A, dragDropLocal = 0x4B
    case subscribe = 0x50, frame = 0x51, requestKeyframe = 0x52, pause = 0x53, frameAck = 0x54, corners = 0x55
    case audioSubscribe = 0x60, audio = 0x61
}

let protocolVersion: UInt16 = 1

/// Builds one length-prefixed message.
struct MsgWriter {
    private(set) var body = Data()

    init(_ type: Msg) { body.append(type.rawValue) }

    mutating func u8(_ v: UInt8) { body.append(v) }
    mutating func u16(_ v: UInt16) { withUnsafeBytes(of: v.littleEndian) { body.append(contentsOf: $0) } }
    mutating func u32(_ v: UInt32) { withUnsafeBytes(of: v.littleEndian) { body.append(contentsOf: $0) } }
    mutating func i32(_ v: Int32) { withUnsafeBytes(of: v.littleEndian) { body.append(contentsOf: $0) } }
    mutating func u64(_ v: UInt64) { withUnsafeBytes(of: v.littleEndian) { body.append(contentsOf: $0) } }
    mutating func f32(_ v: Float) { u32(v.bitPattern) }
    mutating func f32(_ v: CGFloat) { f32(Float(v)) }
    mutating func str(_ s: String) {
        let d = Data(s.utf8.prefix(65535))
        u16(UInt16(d.count)); body.append(d)
    }
    mutating func bytes(_ d: Data) { u32(UInt32(d.count)); body.append(d) }

    /// The framed message: u32 length prefix followed by type + payload.
    var framed: Data {
        var out = Data(capacity: body.count + 4)
        withUnsafeBytes(of: UInt32(body.count).littleEndian) { out.append(contentsOf: $0) }
        out.append(body)
        return out
    }
}

struct WireError: Error { let what: String }

/// Parses the payload of a single message.
struct MsgReader {
    let type: Msg
    private let data: Data
    private var pos: Int

    init?(_ message: Data) {
        guard let first = message.first, let t = Msg(rawValue: first) else { return nil }
        type = t; data = message; pos = message.startIndex + 1
    }

    private mutating func take(_ n: Int) throws -> Data {
        guard pos + n <= data.endIndex else { throw WireError(what: "short \(type)") }
        defer { pos += n }
        return data[pos..<pos + n]
    }
    private mutating func int<T: FixedWidthInteger>(_: T.Type) throws -> T {
        let d = try take(MemoryLayout<T>.size)
        var v: T = 0
        withUnsafeMutableBytes(of: &v) { $0.copyBytes(from: d) }
        return T(littleEndian: v)
    }
    mutating func u8() throws -> UInt8 { try int(UInt8.self) }
    mutating func u16() throws -> UInt16 { try int(UInt16.self) }
    mutating func u32() throws -> UInt32 { try int(UInt32.self) }
    mutating func i32() throws -> Int32 { try int(Int32.self) }
    mutating func u64() throws -> UInt64 { try int(UInt64.self) }
    mutating func f32() throws -> CGFloat { CGFloat(Float(bitPattern: try u32())) }
    mutating func str() throws -> String { String(decoding: try take(Int(try u16())), as: UTF8.self) }
    mutating func bytes() throws -> Data { Data(try take(Int(try u32()))) }
}

/// Accumulates stream bytes and splits them into whole messages.
struct FrameSplitter {
    private var buf = Data()

    mutating func push(_ d: Data) -> [Data] {
        buf.append(d)
        var out: [Data] = []
        while buf.count >= 4 {
            let s = buf.startIndex
            let len = Int(UInt32(buf[s]) | UInt32(buf[s + 1]) << 8 | UInt32(buf[s + 2]) << 16 | UInt32(buf[s + 3]) << 24)
            guard buf.count >= 4 + len else { break }
            out.append(Data(buf[(s + 4)..<(s + 4 + len)]))
            buf.removeFirst(4 + len)
        }
        return out
    }
}
