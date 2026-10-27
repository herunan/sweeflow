import Foundation
import Network
import Security
import CryptoKit

/// One QUIC stream with message framing and send-side byte accounting (for backpressure).
final class MsgStream {
    let conn: NWConnection
    private let queue: DispatchQueue
    private var splitter = FrameSplitter()
    private(set) var pendingBytes = 0
    var onMessage: ((MsgReader) -> Void)?
    var onClose: (() -> Void)?
    private var closed = false

    init(_ conn: NWConnection, queue: DispatchQueue) {
        self.conn = conn
        self.queue = queue
    }

    func start() {
        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .failed, .cancelled: self?.finish()
            default: break
            }
        }
        conn.start(queue: queue)
        receive()
    }

    private func receive() {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) { [weak self] data, _, done, err in
            guard let self else { return }
            if let data {
                for m in self.splitter.push(data) {
                    if let r = MsgReader(m) { self.onMessage?(r) }
                }
            }
            if done || err != nil { self.finish() } else { self.receive() }
        }
    }

    func send(_ w: MsgWriter) { send(w.framed) }

    func send(_ d: Data) {
        guard !closed else { return }
        pendingBytes += d.count
        conn.send(content: d, completion: .contentProcessed { [weak self] _ in
            self?.pendingBytes -= d.count
        })
    }

    func close() {
        guard !closed else { return }
        conn.cancel()
        finish()
    }

    private func finish() {
        guard !closed else { return }
        closed = true
        onClose?()
    }
}

/// QUIC listener. Accepts one client session at a time (a new one replaces the old).
final class Server {
    let queue = DispatchQueue(label: "sweeflow.net", qos: .userInteractive)
    private let identity: Identity
    private var listener: NWListener?
    private var session: Session?
    private var groups: [ObjectIdentifier: NWConnectionGroup] = [:]
    /// The client certificate of the latest handshake (one client connects at a time).
    private var lastVerified: String?

    /// Ends the connection of that client, if it's the one connected.
    func drop(_ fingerprint: String, telling msg: Msg) {
        queue.async { [weak self] in
            guard let s = self?.session, s.clientFingerprint == fingerprint else { return }
            s.sendAndClose(msg)
        }
    }
    let makeSession: (NWConnectionGroup, DispatchQueue) -> Session

    init(identity: Identity, makeSession: @escaping (NWConnectionGroup, DispatchQueue) -> Session) {
        self.identity = identity
        self.makeSession = makeSession
    }

    func start(port: UInt16) throws {
        let quic = NWProtocolQUIC.Options(alpn: ["sweeflow/1"])
        quic.idleTimeout = 15_000
        // Cumulative, not concurrent: Network.framework doesn't raise it as streams close, and
        // each window and popup opens one (a video-call app's tooltips churn through dozens). At 64 the
        // client's next stream blocked, and with it every click.
        quic.initialMaxStreamsBidirectional = 1_000_000
        quic.initialMaxData = 64 << 20
        quic.initialMaxStreamDataBidirectionalLocal = 16 << 20
        quic.initialMaxStreamDataBidirectionalRemote = 16 << 20
        // Video goes as datagrams (a lost packet drops its frame instead of stalling the rest).
        quic.maxDatagramFrameSize = 65535
        let sec = quic.securityProtocolOptions
        sec_protocol_options_set_local_identity(sec, identity.secIdentity)
        sec_protocol_options_set_peer_authentication_required(sec, true)
        let authorized = identity.authorizedClients
        sec_protocol_options_set_verify_block(sec, { [weak self] _, trust, complete in
            let t = sec_trust_copy_ref(trust).takeRetainedValue()
            guard let chain = SecTrustCopyCertificateChain(t) as? [SecCertificate], let leaf = chain.first else {
                return complete(false)
            }
            let fp = SHA256.hash(data: SecCertificateCopyData(leaf) as Data).map { String(format: "%02x", $0) }.joined()
            let ok = authorized().contains(fp)
            if !ok { log("rejected client certificate \(fp)") } else { self?.lastVerified = fp }
            complete(ok)
        }, queue)

        let params = NWParameters(quic: quic)
        params.serviceClass = .interactiveVideo
        let l = try NWListener(using: params, on: NWEndpoint.Port(rawValue: port)!)
        // Findable on the network by name, so Linux keeps finding the Mac when its address changes.
        l.service = NWListener.Service(name: macName, type: streamServiceType, domain: nil, txtRecord: serviceTXT())
        l.stateUpdateHandler = { log("listener: \($0)") }
        l.newConnectionGroupHandler = { [weak self] group in self?.accept(group) }
        l.start(queue: queue)
        listener = l
    }

    private func accept(_ group: NWConnectionGroup) {
        log("client connecting")
        let key = ObjectIdentifier(group)
        groups[key] = group
        let s = makeSession(group, queue)
        s.clientFingerprint = lastVerified
        group.stateUpdateHandler = { [weak self, weak s] state in
            log("connection: \(state)")
            switch state {
            case .failed, .cancelled:
                s?.shutdown()
                self?.groups[key] = nil
                if self?.session === s { self?.session = nil }
            default: break
            }
        }
        group.newConnectionHandler = { [weak s] conn in s?.addStream(conn) }
        session?.shutdown()
        session = s
        group.start(queue: queue)
    }
}

/// TLS identity plus the list of paired client certificate fingerprints.
struct Identity {
    let secIdentity: sec_identity_t
    let fingerprint: String
    let authorizedClients: () -> Set<String>

    static let dir = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/sweeflow")

    /// Loads (or creates on first run) the host certificate. Uses the system LibreSSL because
    /// Security.framework has no API to mint a self-signed certificate.
    static func loadOrCreate() throws -> Identity {
        let fm = FileManager.default
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        let p12 = dir.appendingPathComponent("host.p12")
        let cert = dir.appendingPathComponent("host.pem")
        if !fm.fileExists(atPath: p12.path) {
            let key = dir.appendingPathComponent("host.key")
            let cnf = dir.appendingPathComponent("openssl.cnf")
            try """
            [req]
            distinguished_name=dn
            x509_extensions=v3
            [dn]
            [v3]
            basicConstraints=critical,CA:FALSE
            subjectAltName=DNS:sweeflow
            keyUsage=critical,digitalSignature,keyEncipherment
            extendedKeyUsage=serverAuth,clientAuth
            """.write(to: cnf, atomically: true, encoding: .utf8)
            try run("/usr/bin/openssl", ["req", "-x509", "-config", cnf.path, "-newkey", "rsa:2048", "-nodes",
                                         "-keyout", key.path, "-out", cert.path, "-days", "36500", "-subj", "/CN=sweeflow"])
            try run("/usr/bin/openssl", ["pkcs12", "-export", "-inkey", key.path, "-in", cert.path, "-out", p12.path,
                                         "-passout", "pass:sweeflow", "-keypbe", "PBE-SHA1-3DES", "-certpbe", "PBE-SHA1-3DES",
                                         "-macalg", "sha1"])
            try fm.setAttributes([.posixPermissions: 0o600], ofItemAtPath: key.path)
            try fm.setAttributes([.posixPermissions: 0o600], ofItemAtPath: p12.path)
        }
        var items: CFArray?
        let status = SecPKCS12Import(try Data(contentsOf: p12) as CFData,
                                     [kSecImportExportPassphrase as String: "sweeflow",
                                      kSecImportToMemoryOnly as String: true] as CFDictionary, &items)
        guard status == errSecSuccess, let first = (items as? [[String: Any]])?.first,
              let ident = first[kSecImportItemIdentity as String] else {
            throw WireError(what: "could not load host identity (\(status))")
        }
        let secIdent = ident as! SecIdentity
        var leaf: SecCertificate?
        SecIdentityCopyCertificate(secIdent, &leaf)
        let fp = SHA256.hash(data: SecCertificateCopyData(leaf!) as Data).map { String(format: "%02x", $0) }.joined()
        try fp.write(to: dir.appendingPathComponent("fingerprint"), atomically: true, encoding: .utf8)

        return Identity(secIdentity: sec_identity_create(secIdent)!, fingerprint: fp, authorizedClients: {
            // Re-read on every handshake so pairing a new client needs no restart. Forgotten ones
            // still get in once, to be told (and forget this Mac too).
            Set(pairedClients().map(\.fingerprint)).union(forgottenClients())
        })
    }

    static let clientsFile = dir.appendingPathComponent("clients")

    /// Paired clients, one per line: `<sha256 hex> [name]` (older lines have no name).
    /// Once each: a machine listed twice (an older setup added it again, without its name)
    /// keeps the line with its name.
    static func pairedClients() -> [(fingerprint: String, name: String)] {
        let text = (try? String(contentsOf: clientsFile, encoding: .utf8)) ?? ""
        var out: [(fingerprint: String, name: String)] = []
        for line in text.split(whereSeparator: \.isNewline) {
            let l = line.trimmingCharacters(in: .whitespaces)
            guard !l.isEmpty, !l.hasPrefix("#") else { continue }
            let parts = l.split(separator: " ", maxSplits: 1)
            let fp = parts[0].lowercased(), name = parts.count > 1 ? String(parts[1]) : ""
            if let i = out.firstIndex(where: { $0.fingerprint == fp }) {
                if out[i].name.isEmpty { out[i].name = name }
            } else {
                out.append((fp, name))
            }
        }
        return out
    }

    static func addClient(_ fingerprint: String, name: String) {
        var all = pairedClients().filter { $0.fingerprint != fingerprint.lowercased() }
        all.append((fingerprint.lowercased(), name))
        writeClients(all)
    }

    static func removeClient(_ fingerprint: String) {
        writeClients(pairedClients().filter { $0.fingerprint != fingerprint })
    }

    static func renameClient(_ fingerprint: String, to name: String) {
        writeClients(pairedClients().map { $0.fingerprint == fingerprint ? ($0.fingerprint, name) : $0 })
    }

    /// What a paired client is called here.
    static func clientName(_ fingerprint: String) -> String {
        let n = pairedClients().first { $0.fingerprint == fingerprint }?.name ?? ""
        return n.isEmpty ? "Linux \(PairingCode.of(fingerprint: fingerprint))" : n
    }

    // Lists of fingerprints, one per line, beside the clients file.
    private static func list(_ name: String) -> Set<String> {
        let text = (try? String(contentsOf: dir.appendingPathComponent(name), encoding: .utf8)) ?? ""
        return Set(text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces).lowercased() }.filter { !$0.isEmpty })
    }

    private static func setList(_ name: String, _ items: Set<String>) {
        try? (items.sorted().joined(separator: "\n") + "\n").write(to: dir.appendingPathComponent(name), atomically: true, encoding: .utf8)
    }

    /// Clients disconnected from this Mac's menu: refused until connected again.
    static func disconnectedClients() -> Set<String> { list("disconnected") }
    static func setDisconnected(_ fingerprint: String, _ on: Bool) {
        var s = disconnectedClients()
        if on { s.insert(fingerprint) } else { s.remove(fingerprint) }
        setList("disconnected", s)
    }

    /// Clients forgotten here, still to be told (on their next connection).
    static func forgottenClients() -> Set<String> { list("forgotten") }
    static func setForgotten(_ fingerprint: String, _ on: Bool) {
        var s = forgottenClients()
        if on { s.insert(fingerprint) } else { s.remove(fingerprint) }
        setList("forgotten", s)
    }

    private static func writeClients(_ all: [(fingerprint: String, name: String)]) {
        let text = all.map { $0.name.isEmpty ? $0.fingerprint : "\($0.fingerprint) \($0.name)" }.joined(separator: "\n") + "\n"
        try? text.write(to: clientsFile, atomically: true, encoding: .utf8)
    }
}

@discardableResult
func run(_ path: String, _ args: [String]) throws -> String {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: path)
    p.arguments = args
    let out = Pipe()
    p.standardOutput = out
    p.standardError = FileHandle.nullDevice
    try p.run()
    let data = out.fileHandleForReading.readDataToEndOfFile()
    p.waitUntilExit()
    guard p.terminationStatus == 0 else { throw WireError(what: "\(path) failed (\(p.terminationStatus))") }
    return String(decoding: data, as: UTF8.self)
}

func log(_ s: String) {
    let ts = String(format: "%.3f", Date().timeIntervalSince1970.truncatingRemainder(dividingBy: 1000))
    FileHandle.standardError.write(Data("[\(ts)] \(s)\n".utf8))
}
