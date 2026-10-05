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
    let makeSession: (NWConnectionGroup, DispatchQueue) -> Session

    init(identity: Identity, makeSession: @escaping (NWConnectionGroup, DispatchQueue) -> Session) {
        self.identity = identity
        self.makeSession = makeSession
    }

    func start(port: UInt16) throws {
        let quic = NWProtocolQUIC.Options(alpn: ["sweeflow/1"])
        quic.idleTimeout = 15_000
        quic.initialMaxStreamsBidirectional = 64
        quic.initialMaxData = 64 << 20
        quic.initialMaxStreamDataBidirectionalLocal = 16 << 20
        quic.initialMaxStreamDataBidirectionalRemote = 16 << 20
        // Video goes as datagrams (a lost packet drops its frame instead of stalling the rest).
        quic.maxDatagramFrameSize = 65535
        let sec = quic.securityProtocolOptions
        sec_protocol_options_set_local_identity(sec, identity.secIdentity)
        sec_protocol_options_set_peer_authentication_required(sec, true)
        let authorized = identity.authorizedClients
        sec_protocol_options_set_verify_block(sec, { _, trust, complete in
            let t = sec_trust_copy_ref(trust).takeRetainedValue()
            guard let chain = SecTrustCopyCertificateChain(t) as? [SecCertificate], let leaf = chain.first else {
                return complete(false)
            }
            let fp = SHA256.hash(data: SecCertificateCopyData(leaf) as Data).map { String(format: "%02x", $0) }.joined()
            let ok = authorized().contains(fp)
            if !ok { log("rejected client certificate \(fp)") }
            complete(ok)
        }, queue)

        let params = NWParameters(quic: quic)
        params.serviceClass = .interactiveVideo
        let l = try NWListener(using: params, on: NWEndpoint.Port(rawValue: port)!)
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

        let clients = dir.appendingPathComponent("clients")
        return Identity(secIdentity: sec_identity_create(secIdent)!, fingerprint: fp, authorizedClients: {
            // Re-read on every handshake so pairing a new client needs no restart.
            let text = (try? String(contentsOf: clients, encoding: .utf8)) ?? ""
            return Set(text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces).lowercased() }
                .filter { !$0.isEmpty && !$0.hasPrefix("#") })
        })
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
