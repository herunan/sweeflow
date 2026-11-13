import AppKit
import Network
import CryptoKit

/// A short name for a linked computer that gave none: the first 40 bits of its certificate
/// fingerprint in Crockford base32, `XXXX-XXXX` (see Identity.clientName).
enum PairingCode {
    private static let alphabet = Array("0123456789ABCDEFGHJKMNPQRSTVWXYZ")

    static func of(fingerprint hex: String) -> String {
        let h = Array(hex.lowercased().utf8.prefix(10))
        guard h.count == 10, let v = UInt64(String(decoding: h, as: UTF8.self), radix: 16) else { return "????-????" }
        let chars = (0..<8).map { alphabet[Int((v >> UInt64(35 - 5 * $0)) & 31)] }
        return String(chars[0..<4]) + "-" + String(chars[4..<8])
    }
}

/// The code both screens show while linking, `XXX-XXX`. Each side works it out from what it sees
/// of the connection itself: both certificates' fingerprints and a fresh nonce from each side. A
/// computer answering in between holds different certificates on its two sides, so its two codes
/// differ. The Linux client's `link_code` is the same sum (linux/src/pair.rs).
enum LinkCode {
    /// No 0 or O, no 1, I or L, which read alike.
    private static let alphabet = Array("ABCDEFGHJKMNPQRSTUVWXYZ23456789")

    /// SHA-256 over a label, the two fingerprints (hex) and the two nonces; its first 8 bytes as
    /// a number, written as 6 characters of the alphabet.
    static func of(client: String, host: String, clientNonce: Data, hostNonce: Data) -> String {
        var h = SHA256()
        h.update(data: Data("sweeflow-link/2".utf8))
        h.update(data: Data(client.lowercased().utf8))
        h.update(data: Data(host.lowercased().utf8))
        h.update(data: clientNonce)
        h.update(data: hostNonce)
        var v: UInt64 = 0
        for byte in Array(h.finalize()).prefix(8) { v = v << 8 | UInt64(byte) }
        let n = UInt64(alphabet.count)
        v %= n * n * n * n * n * n
        var chars: [Character] = []
        for _ in 0..<6 {
            chars.append(alphabet[Int(v % n)])
            v /= n
        }
        chars.reverse()
        return String(chars[0..<3]) + "-" + String(chars[3..<6])
    }
}

/// This Mac as people know it (System Settings → General → Sharing → Computer name).
let macName = Host.current().localizedName ?? "Mac"

/// Bonjour service types: one for the streaming listener, one for the linking listener. Both are
/// findable whenever Sweeflow runs.
let streamServiceType = "_sweeflow._udp"
let pairServiceType = "_sweeflow-pair._udp"

/// How a link request ended, as PAIR_RESULT says it (PROTOCOL.md).
enum PairStatus: UInt8 {
    case linked = 0, refused = 1, busy = 2, wait = 3, locked = 4, alreadyLinked = 5, version = 6
}

/// Linking: a second QUIC listener, up whenever Sweeflow runs, that takes any client certificate.
/// A computer asks, both sides work out the same code (LinkCode), the person at the Mac is asked
/// to allow it, and it's added to the paired clients. Apart from the question itself, all of it
/// happens on this class's own queue, away from a streaming session's.
final class Pairing {
    static let shared = Pairing()
    static let port: UInt16 = 44121
    /// The exchange's version: in the listener's ALPN, and checked again in each request.
    static let version: UInt16 = 2
    /// How long a computer that was refused can't ask again.
    private static let refusedFor: TimeInterval = 300

    private let queue = DispatchQueue(label: "sweeflow.pairing")
    private var listener: NWListener?
    private var groups: [ObjectIdentifier: NWConnectionGroup] = [:]
    /// Open request streams (kept here: nothing else holds them while the person decides).
    private var streams: [ObjectIdentifier: MsgStream] = [:]
    /// Fingerprint of the last client certificate seen (fallback if a stream lacks TLS metadata).
    /// Should it ever be another connection's, the two codes differ and nothing is linked.
    private var lastPeer: String?
    /// Requests that have this Mac's commitment and still owe their own nonce, by stream.
    private var pending: [ObjectIdentifier: Request] = [:]
    /// The stream whose computer the person at the Mac is being asked about.
    private var asking: ObjectIdentifier?
    /// Computers the person refused, and when.
    private var refused: [String: Date] = [:]
    /// When a question was last taken down because its computer left. The next one waits a
    /// moment: each question shows one code, and nobody gets to run through codes quickly.
    private var withdrawn = Date.distantPast

    private struct Request {
        let fingerprint: String
        let name: String
        /// This Mac's nonce: committed to, shown once the computer has sent its own.
        let nonce: Data
    }

    /// Starts taking link requests. Called once, when Sweeflow starts.
    func start() { queue.async { self.listen() } }

    private func listen() {
        guard listener == nil else { return }
        let quic = NWProtocolQUIC.Options(alpn: ["sweeflow-pair/2"])
        // The computer that asks keeps the connection alive while the person decides; one that
        // vanished is noticed after this long, and its question taken down.
        quic.idleTimeout = 30_000
        // The default allows no client streams at all; the client opens one.
        quic.initialMaxStreamsBidirectional = 4
        quic.initialMaxStreamDataBidirectionalRemote = 64 << 10
        quic.initialMaxData = 256 << 10
        let sec = quic.securityProtocolOptions
        sec_protocol_options_set_local_identity(sec, identity.secIdentity)
        sec_protocol_options_set_peer_authentication_required(sec, true)
        // Anyone may ask; the person at the Mac decides, by the code.
        sec_protocol_options_set_verify_block(sec, { [weak self] _, trust, complete in
            let t = sec_trust_copy_ref(trust).takeRetainedValue()
            if let leaf = (SecTrustCopyCertificateChain(t) as? [SecCertificate])?.first {
                self?.lastPeer = Self.fingerprint(leaf)
            }
            complete(true)
        }, queue)
        do {
            let l = try NWListener(using: NWParameters(quic: quic), on: NWEndpoint.Port(rawValue: Pairing.port)!)
            l.service = NWListener.Service(name: macName, type: pairServiceType, domain: nil,
                                           txtRecord: serviceTXT())
            l.stateUpdateHandler = { [weak self] state in
                log("pairing listener: \(state)")
                // It's meant to be up for as long as Sweeflow runs: one that failed is started again.
                if case .failed = state { self?.listenAgain() }
            }
            l.newConnectionGroupHandler = { [weak self] group in self?.accept(group) }
            l.start(queue: queue)
            listener = l
        } catch {
            log("pairing listener failed: \(error)")
            listenAgain()
        }
    }

    private func listenAgain() {
        listener?.cancel()
        listener = nil
        queue.asyncAfter(deadline: .now() + 10) { [weak self] in self?.listen() }
    }

    private func accept(_ group: NWConnectionGroup) {
        log("pairing: a computer is connecting")
        let key = ObjectIdentifier(group)
        groups[key] = group
        group.stateUpdateHandler = { [weak self] state in
            log("pairing connection: \(state)")
            if case .failed = state { self?.groups[key] = nil }
            if case .cancelled = state { self?.groups[key] = nil }
        }
        group.newConnectionHandler = { [weak self] conn in self?.handle(conn) }
        group.start(queue: queue)
    }

    private func handle(_ conn: NWConnection) {
        log("pairing: stream opened")
        let stream = MsgStream(conn, queue: queue)
        let key = ObjectIdentifier(stream)
        streams[key] = stream
        stream.onClose = { [weak self] in
            guard let self else { return }
            self.streams[key] = nil
            self.pending[key] = nil
            // That computer gave up: don't leave the question up for nobody.
            if self.asking == key {
                self.asking = nil
                self.withdrawn = Date()
                DispatchQueue.main.async { LinkPrompt.shared.withdraw() }
            }
        }
        stream.onMessage = { [weak self, weak stream] m in
            guard let self, let stream else { return }
            switch m.type {
            case .pairRequest: self.request(m, on: stream, conn)
            case .pairNonce: self.reveal(m, on: stream)
            default: log("pairing: unexpected message \(m.type)")
            }
        }
        stream.start()
    }

    /// A computer asks to link. Answered at once when there's nothing to ask the person;
    /// otherwise with this Mac's commitment to a nonce, the first half of settling the code.
    private func request(_ message: MsgReader, on stream: MsgStream, _ conn: NWConnection) {
        var m = message
        let key = ObjectIdentifier(stream)
        guard pending[key] == nil, asking != key else { return log("pairing: a second request on one stream") }
        guard let version = try? m.u16() else { return log("pairing: malformed request") }
        guard version == Pairing.version else {
            log("pairing: a request in version \(version), this Mac speaks \(Pairing.version)")
            return reply(stream, .version)
        }
        guard let said = try? m.str(), let known = try? m.u8() else { return log("pairing: malformed request") }
        guard let fp = (peerFingerprint(conn) ?? lastPeer)?.lowercased() else {
            log("pairing: no client certificate")
            return reply(stream, .refused)
        }
        let name = Self.shownName(said)
        log("pairing: \(name) (\(fp.prefix(8))) asks to link")
        // Linked already, and it has this Mac too: nothing to ask, and nothing to add. (One that
        // lost this Mac is asked about again: the code is all it has to know this Mac by.)
        if known != 0, Identity.pairedClients().contains(where: { $0.fingerprint == fp }) {
            log("pairing: \(name) is linked already")
            return reply(stream, .alreadyLinked)
        }
        if Self.screenLocked() {
            log("pairing: the screen is locked")
            return reply(stream, .locked)
        }
        let now = Date()
        refused = refused.filter { now.timeIntervalSince($0.value) < Self.refusedFor }
        if refused[fp] != nil {
            log("pairing: \(name) was refused a moment ago")
            return reply(stream, .wait)
        }
        guard canAsk(now) else { return reply(stream, .busy) }
        let nonce = Data((0..<32).map { _ in UInt8.random(in: UInt8.min ... UInt8.max) })
        pending[key] = Request(fingerprint: fp, name: name, nonce: nonce)
        var w = MsgWriter(.pairCommit)
        w.bytes(Data(SHA256.hash(data: nonce)))
        stream.send(w)
    }

    /// The computer's own nonce, sent without having seen this Mac's. Both sides now have the
    /// code: this Mac's nonce goes back, and the person is asked.
    private func reveal(_ message: MsgReader, on stream: MsgStream) {
        var m = message
        let key = ObjectIdentifier(stream)
        guard let r = pending.removeValue(forKey: key) else { return log("pairing: a nonce nobody asked for") }
        guard let theirs = try? m.bytes(), theirs.count == 32 else { return log("pairing: malformed nonce") }
        // Another computer got to the question first.
        guard canAsk(Date()) else { return reply(stream, .busy) }
        asking = key
        let code = LinkCode.of(client: r.fingerprint, host: identity.fingerprint, clientNonce: theirs, hostNonce: r.nonce)
        var w = MsgWriter(.pairReveal)
        w.bytes(r.nonce)
        stream.send(w)
        log("pairing: asking whether to link \(r.name), code \(code)")
        DispatchQueue.main.async {
            LinkPrompt.shared.ask(name: r.name, code: code) { allow in
                self.queue.async {
                    guard self.asking == key else { return }   // gone meanwhile
                    self.asking = nil
                    if allow { self.link(r) } else { self.refused[r.fingerprint] = Date() }
                    log("pairing: \(r.name) \(allow ? "allowed" : "not allowed")")
                    self.reply(stream, allow ? .linked : .refused)
                }
            }
        }
    }

    /// The person allowed it: it may connect from now on.
    private func link(_ r: Request) {
        // One this Mac still had (unlinked on the Linux side only) keeps the name it has here.
        if !Identity.pairedClients().contains(where: { $0.fingerprint == r.fingerprint }) {
            Identity.addClient(r.fingerprint, name: r.name)
        }
        // Left over from before it was linked again: forgotten here but never told so (it would
        // be told on connecting, and forget this Mac), or disconnected from the menu.
        if Identity.forgottenClients().contains(r.fingerprint) { Identity.setForgotten(r.fingerprint, false) }
        if Identity.disconnectedClients().contains(r.fingerprint) { Identity.setDisconnected(r.fingerprint, false) }
    }

    /// No question is up, and none was taken down just now (see `withdrawn`).
    private func canAsk(_ now: Date) -> Bool {
        asking == nil && now.timeIntervalSince(withdrawn) >= 3
    }

    private func reply(_ stream: MsgStream, _ status: PairStatus) {
        var w = MsgWriter(.pairResult)
        w.u8(status.rawValue)
        w.str(macName)
        w.u16(settings.config.port)
        w.u16(Pairing.version)
        stream.send(w)
    }

    private func peerFingerprint(_ conn: NWConnection) -> String? {
        guard let md = conn.metadata(definition: NWProtocolQUIC.definition) as? NWProtocolQUIC.Metadata else { return nil }
        var fp: String?
        sec_protocol_metadata_access_peer_certificate_chain(md.securityProtocolMetadata) { cert in
            if fp == nil { fp = Self.fingerprint(sec_certificate_copy_ref(cert).takeRetainedValue()) }
        }
        return fp
    }

    static func fingerprint(_ cert: SecCertificate) -> String {
        SHA256.hash(data: SecCertificateCopyData(cert) as Data).map { String(format: "%02x", $0) }.joined()
    }

    /// The name a computer gives itself, fit to show and to keep: one line (the clients file has
    /// a line per computer), of a sensible length.
    static func shownName(_ said: String) -> String {
        let name = String(said.filter { !$0.isNewline }.prefix(60)).trimmingCharacters(in: .whitespaces)
        return name.isEmpty ? "Linux machine" : name
    }

    /// Asked afresh each time: ScreenLock only follows the lock once a session has streamed.
    private static func screenLocked() -> Bool {
        let d = CGSessionCopyCurrentDictionary() as? [String: Any]
        return d?["CGSSessionScreenIsLocked"] as? Bool ?? false
    }
}

/// TXT record for both services: which Mac this is (stable across renames and IP changes),
/// the streaming port and the protocol version.
func serviceTXT() -> NWTXTRecord {
    NWTXTRecord(["id": String(identity.fingerprint.prefix(16)), "port": "\(settings.config.port)", "v": "\(protocolVersion)"])
}

/// The question a link request puts to the person at the Mac, laid out like an alert. A window
/// of its own rather than a modal alert: nothing else in Sweeflow waits on it (a session may be
/// streaming), and it's taken down when the other computer stops waiting. Main thread only.
final class LinkPrompt: NSObject {
    static let shared = LinkPrompt()
    private var window: NSWindow?
    private var done: ((Bool) -> Void)?

    var showing: Bool { window != nil }

    /// Asks whether to link the computer called `name`. `done` gets the answer, unless the
    /// question is withdrawn first.
    func ask(name: String, code: String, done: @escaping (Bool) -> Void) {
        withdraw()
        // The explanation has done its job once a computer asks.
        PairingWindow.shared.close()
        let icon = NSImageView(image: NSApp.applicationIconImage)
        icon.widthAnchor.constraint(equalToConstant: 56).isActive = true
        icon.heightAnchor.constraint(equalToConstant: 56).isActive = true
        let title = NSTextField(wrappingLabelWithString: "\(name) wants to link to this Mac")
        title.font = .systemFont(ofSize: 15, weight: .semibold)
        title.alignment = .center
        title.preferredMaxLayoutWidth = 300
        let shown = NSTextField(labelWithString: code)
        shown.font = .monospacedSystemFont(ofSize: 36, weight: .medium)
        let how = NSTextField(wrappingLabelWithString: "Allow only if \(name) shows the same code.")
        how.alignment = .center
        how.textColor = .secondaryLabelColor
        how.preferredMaxLayoutWidth = 300
        // Allow has no key of its own: a key meant for another app must never link a computer.
        let no = NSButton(title: "Don't Allow", target: self, action: #selector(clickedDontAllow))
        no.keyEquivalent = "\u{1b}"
        let yes = NSButton(title: "Allow", target: self, action: #selector(clickedAllow))
        let buttons = NSStackView(views: [no, yes])
        buttons.orientation = .horizontal
        buttons.spacing = 12
        let stack = NSStackView(views: [icon, title, shown, how, buttons])
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 14
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 28, bottom: 20, right: 28)
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 360, height: 300), styleMask: [.titled],
                         backing: .buffered, defer: false)
        w.title = "Sweeflow"
        w.contentView = stack
        w.isReleasedWhenClosed = false
        // Above other windows, as an alert would be.
        w.level = .modalPanel
        window = w
        self.done = done
        presentToPerson(w)
    }

    /// Takes the question down unanswered (the computer that asked is gone).
    func withdraw() {
        done = nil
        close()
    }

    private func close() {
        guard let w = window else { return }
        window = nil
        w.close()
        BuiltInScreen.resume()
    }

    @objc private func clickedAllow() { answer(true) }
    @objc private func clickedDontAllow() { answer(false) }

    private func answer(_ allow: Bool) {
        let done = self.done
        self.done = nil
        close()
        done?(allow)
    }
}

/// What "Link a Linux machine…" opens: linking is started on the Linux machine, so this only
/// says how, and what to type there if this Mac isn't found by itself. Main thread only.
final class PairingWindow: NSObject, NSWindowDelegate {
    static let shared = PairingWindow()
    private var window: NSWindow?

    func show() {
        // Built each time: this Mac's addresses change with the network.
        window?.close()
        let w = build()
        window = w
        presentToPerson(w)
    }

    func close() { window?.close() }

    private func build() -> NSWindow {
        let title = NSTextField(labelWithString: "Link a Linux machine")
        title.font = .systemFont(ofSize: 17, weight: .semibold)
        let how = NSTextField(wrappingLabelWithString:
            "Linking is started from the Linux machine. Open Sweeflow there, choose “Link a Mac…” and pick this Mac. "
            + "This Mac then asks you to allow it.")
        how.preferredMaxLayoutWidth = 340
        let manual = NSTextField(wrappingLabelWithString: "If this Mac isn't found automatically, type its address there.")
        manual.preferredMaxLayoutWidth = 340
        let addresses = Self.addresses()
        let address = addresses.isEmpty ? "none, this Mac isn't on a network" : addresses.joined(separator: " or ")
        let details = NSTextField(wrappingLabelWithString: "Name: \(macName)\nAddress: \(address)")
        details.preferredMaxLayoutWidth = 340
        details.isSelectable = true
        details.textColor = .secondaryLabelColor
        let done = NSButton(title: "Done", target: self, action: #selector(clickedDone))
        done.keyEquivalent = "\r"
        let stack = NSStackView(views: [title, how, manual, details, done])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 28, bottom: 20, right: 28)
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 280), styleMask: [.titled, .closable],
                         backing: .buffered, defer: false)
        w.title = "Sweeflow"
        w.contentView = stack
        w.isReleasedWhenClosed = false
        w.delegate = self
        return w
    }

    /// This Mac's IPv4 addresses another computer can use: not its own loopback, nor the ones
    /// made up on a network that hands none out.
    private static func addresses() -> [String] {
        Host.current().addresses.filter {
            $0.contains(".") && !$0.contains(":") && !$0.hasPrefix("127.") && !$0.hasPrefix("169.254.")
        }
    }

    @objc private func clickedDone() { window?.close() }

    func windowWillClose(_ notification: Notification) {
        window = nil
        // The question, if it's up, keeps the screen lit until it's answered.
        if !LinkPrompt.shared.showing { BuiltInScreen.resume() }
    }
}
