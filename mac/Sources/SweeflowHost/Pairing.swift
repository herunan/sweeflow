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

/// The one-time code this Mac shows while a computer links: 4 digits. Short, because it can only
/// be tried by talking to this Mac (LinkPake: nothing seen lets anyone test codes offline), three
/// times per code, each code needs its own window on this screen, and wrong codes in a row make
/// this Mac wait longer and longer before it shows another (`Pairing.cooldown`).
enum LinkCode {
    static let digits = 4

    static func random() -> String {
        // SystemRandomNumberGenerator: the system's cryptographic generator.
        (0..<digits).map { _ in String(Int.random(in: 0...9)) }.joined()
    }
}

/// This Mac as people know it (System Settings → General → Sharing → Computer name).
let macName = Host.current().localizedName ?? "Mac"

/// Bonjour service types: one for the streaming listener, one for the linking listener. Both are
/// findable whenever Sweeflow runs.
let streamServiceType = "_sweeflow._udp"
let pairServiceType = "_sweeflow-pair._udp"

/// How a request to the linking listener ended, as PAIR_RESULT says it (PROTOCOL.md).
enum PairStatus: UInt8 {
    case linked = 0, cancelled = 1, busy = 2, wait = 3, locked = 4, alreadyLinked = 5, version = 6
    case tooManyWrong = 7, expired = 8, unlinked = 9, notLinked = 10
}

/// Linking: a second QUIC listener, up whenever Sweeflow runs, that takes any client certificate.
/// A computer asks, this Mac shows a code, the person types it there, and the two run LinkPake
/// with it: once each has proved it has the same key, this Mac adds the computer's certificate to
/// the linked ones. The same listener takes a linked computer's "unlink me" and "am I still
/// linked?". All of it happens on this class's own queue, away from a streaming session's.
final class Pairing {
    static let shared = Pairing()
    static let port: UInt16 = 44121
    /// The exchange's version: in the listener's ALPN, and checked again in each request.
    static let version: UInt16 = 3
    /// Wrong codes allowed for one code shown.
    static let tries = 3
    /// How long a code is shown for.
    private static let shownFor: TimeInterval = 300

    private let queue = DispatchQueue(label: "sweeflow.pairing")
    private var listener: NWListener?
    private var groups: [ObjectIdentifier: NWConnectionGroup] = [:]
    /// Open request streams (kept here: nothing else holds them while the person types).
    private var streams: [ObjectIdentifier: MsgStream] = [:]
    /// Fingerprint of the last client certificate seen (fallback if a stream lacks TLS metadata).
    /// Should it ever be another connection's, the keys differ and nothing is linked; it's never
    /// used to unlink.
    private var lastPeer: String?
    /// The code on screen, and the computer it's for. One at a time.
    private var attempt: Attempt?
    /// Wrong codes since the last computer was linked, and when the last one came.
    private var wrongInARow = 0
    private var lastWrong = Date.distantPast

    private struct Attempt {
        let stream: ObjectIdentifier
        let fingerprint: String
        let name: String
        let code: String
        let generator: Data
        var triesLeft: Int
        /// This Mac's share for the run under way (a fresh one for each try).
        var share: LinkPake.Share
        let id = UUID()
    }

    /// Starts taking link requests. Called once, when Sweeflow starts.
    func start() { queue.async { self.listen() } }

    private func listen() {
        guard listener == nil else { return }
        // Version 2 too, only to tell such a computer to update (it would fail the handshake).
        let quic = NWProtocolQUIC.Options(alpn: ["sweeflow-pair/3", "sweeflow-pair/2"])
        // The computer keeps the connection alive while the person types; one that vanished is
        // noticed after this long, and its code taken down.
        quic.idleTimeout = 30_000
        // The default allows no client streams at all; the client opens them.
        quic.initialMaxStreamsBidirectional = 4
        quic.initialMaxStreamDataBidirectionalRemote = 64 << 10
        quic.initialMaxData = 256 << 10
        let sec = quic.securityProtocolOptions
        sec_protocol_options_set_local_identity(sec, identity.secIdentity)
        sec_protocol_options_set_peer_authentication_required(sec, true)
        // Anyone may ask; only the code links, and it's bound to this certificate (LinkPake).
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
        let stream = MsgStream(conn, queue: queue)
        let key = ObjectIdentifier(stream)
        streams[key] = stream
        stream.onClose = { [weak self] in
            guard let self else { return }
            self.streams[key] = nil
            // That computer gave up: take its code down.
            if self.attempt?.stream == key {
                log("pairing: \(self.attempt!.name) stopped linking")
                self.attempt = nil
                DispatchQueue.main.async { CodeWindow.shared.close() }
            }
        }
        stream.onMessage = { [weak self, weak stream] m in
            guard let self, let stream else { return }
            switch m.type {
            case .pairRequest: self.request(m, on: stream, conn)
            case .pairGuess: self.guess(m, on: stream)
            case .pairUnlink: self.unlink(on: stream, conn)
            case .pairCheck: self.check(on: stream, conn)
            default: log("pairing: unexpected message \(m.type)")
            }
        }
        stream.start()
    }

    /// A computer asks to link. Answered at once when it can't (linked already, locked, another
    /// code up, too many wrong codes lately); otherwise this Mac shows a code and sends its share.
    private func request(_ message: MsgReader, on stream: MsgStream, _ conn: NWConnection) {
        var m = message
        let key = ObjectIdentifier(stream)
        guard attempt?.stream != key else { return log("pairing: a second request on one stream") }
        guard let version = try? m.u16() else { return log("pairing: malformed request") }
        guard version == Pairing.version else {
            log("pairing: a request in version \(version), this Mac speaks \(Pairing.version)")
            return reply(stream, .version)
        }
        guard let said = try? m.str() else { return log("pairing: malformed request") }
        guard let fp = (peerFingerprint(conn) ?? lastPeer)?.lowercased() else {
            log("pairing: no client certificate")
            return reply(stream, .busy)
        }
        let name = Self.shownName(said)
        log("pairing: \(name) (\(fp.prefix(8))) asks to link")
        // Linking again takes unlinking first (on either side, which tells the other).
        if Identity.pairedClients().contains(where: { $0.fingerprint == fp }) {
            log("pairing: \(name) is linked already")
            return reply(stream, .alreadyLinked)
        }
        if Self.screenLocked() {
            log("pairing: the screen is locked")
            return reply(stream, .locked)
        }
        guard attempt == nil else {
            log("pairing: another code is showing")
            return reply(stream, .busy)
        }
        let wait = Self.cooldown(wrongInARow: wrongInARow) - Date().timeIntervalSince(lastWrong)
        if wait > 0 {
            log("pairing: \(wrongInARow) wrong codes in a row, waiting \(Int(wait)) s more")
            return reply(stream, .wait)
        }
        let code = LinkCode.random()
        let g = LinkPake.generator(code: code, client: fp, host: identity.fingerprint)
        guard let share = try? LinkPake.Share.random(generator: g) else { return reply(stream, .busy) }
        let a = Attempt(stream: key, fingerprint: fp, name: name, code: code, generator: g, triesLeft: Self.tries, share: share)
        attempt = a
        sendShare(stream, a)
        log("pairing: showing a code for \(name)")
        DispatchQueue.main.async {
            CodeWindow.shared.show(name: name, code: code) { [weak self] in
                self?.queue.async { self?.end(a.id, .cancelled, "cancelled here") }
            }
        }
        queue.asyncAfter(deadline: .now() + Self.shownFor) { [weak self] in self?.end(a.id, .expired, "the code expired") }
    }

    /// How long to wait after the last wrong code before showing another: nothing for the first
    /// three, then 30 s, doubling with every three more, up to an hour.
    static func cooldown(wrongInARow n: Int) -> TimeInterval {
        n < 3 ? 0 : min(3600, 30 * pow(2, Double(n / 3 - 1)))
    }

    private func sendShare(_ stream: MsgStream, _ a: Attempt) {
        var w = MsgWriter(.pairShare)
        w.u8(UInt8(a.triesLeft))
        w.bytes(a.share.pub)
        stream.send(w)
    }

    /// The computer's share and tag, made with the code the person typed there.
    private func guess(_ message: MsgReader, on stream: MsgStream) {
        var m = message
        let key = ObjectIdentifier(stream)
        guard var a = attempt, a.stream == key else { return log("pairing: a guess nobody asked for") }
        guard let theirs = try? m.bytes(), let tag = try? m.bytes() else { return log("pairing: malformed guess") }
        let tags = try? LinkPake.hostTags(ours: a.share, clientShare: theirs, client: a.fingerprint, host: identity.fingerprint)
        if let tags, LinkPake.same(tags.client, tag) {
            attempt = nil
            wrongInARow = 0
            link(a)
            log("pairing: linked \(a.name)")
            reply(stream, .linked, tag: tags.host)
            DispatchQueue.main.async { CodeWindow.shared.finish("Linked to \(a.name).") }
            return
        }
        wrongInARow += 1
        lastWrong = Date()
        a.triesLeft -= 1
        log("pairing: a wrong code from \(a.name), \(a.triesLeft) tries left")
        guard a.triesLeft > 0, let share = try? LinkPake.Share.random(generator: a.generator) else {
            return end(a.id, .tooManyWrong, "too many wrong codes")
        }
        a.share = share
        attempt = a
        sendShare(stream, a)
        let left = a.triesLeft
        DispatchQueue.main.async { CodeWindow.shared.note("That wasn't the code. \(left) \(left == 1 ? "try" : "tries") left.") }
    }

    /// Stops the attempt `id`, if it's still the one under way, telling its computer why.
    private func end(_ id: UUID, _ status: PairStatus, _ why: String) {
        guard let a = attempt, a.id == id else { return }
        attempt = nil
        log("pairing: stopped linking \(a.name): \(why)")
        if let stream = streams[a.stream] { reply(stream, status) }
        DispatchQueue.main.async {
            switch status {
            case .tooManyWrong: CodeWindow.shared.finish("Too many wrong codes. Linking \(a.name) stopped.")
            case .expired: CodeWindow.shared.finish("The code expired. Linking \(a.name) stopped.")
            default: CodeWindow.shared.close()
            }
        }
    }

    /// Both have proved they have the code: it may connect from now on.
    private func link(_ a: Attempt) {
        Identity.addClient(a.fingerprint, name: a.name)
        // Left over from before it was linked again: forgotten here but never told so (it would
        // be told on connecting, and forget this Mac), or disconnected from the menu.
        if Identity.forgottenClients().contains(a.fingerprint) { Identity.setForgotten(a.fingerprint, false) }
        if Identity.disconnectedClients().contains(a.fingerprint) { Identity.setDisconnected(a.fingerprint, false) }
    }

    /// A linked computer unlinked this Mac: it's forgotten here too. Only ever the computer whose
    /// certificate made this connection (from the handshake itself, never `lastPeer`).
    private func unlink(on stream: MsgStream, _ conn: NWConnection) {
        guard let fp = peerFingerprint(conn)?.lowercased() else { return reply(stream, .busy) }
        if Identity.pairedClients().contains(where: { $0.fingerprint == fp }) {
            log("pairing: \(Identity.clientName(fp)) unlinked this Mac")
            Identity.removeClient(fp)
        }
        if Identity.forgottenClients().contains(fp) { Identity.setForgotten(fp, false) }
        if Identity.disconnectedClients().contains(fp) { Identity.setDisconnected(fp, false) }
        reply(stream, .unlinked)
    }

    /// A computer asks whether it's still linked here (it looks now and then, for a Mac it isn't
    /// streaming from: one unlinked here then forgets this Mac too).
    private func check(on stream: MsgStream, _ conn: NWConnection) {
        guard let fp = peerFingerprint(conn)?.lowercased() else { return reply(stream, .busy) }
        if Identity.pairedClients().contains(where: { $0.fingerprint == fp }) { return reply(stream, .linked) }
        // Told now: no need to tell it when it next connects.
        if Identity.forgottenClients().contains(fp) { Identity.setForgotten(fp, false) }
        reply(stream, .notLinked)
    }

    private func reply(_ stream: MsgStream, _ status: PairStatus, tag: Data = Data()) {
        var w = MsgWriter(.pairResult)
        w.u8(status.rawValue)
        w.str(macName)
        w.u16(settings.config.port)
        w.u16(Pairing.version)
        w.bytes(tag)
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
/// `busy`: streaming to a computer now, so every other linked computer can say so without
/// connecting (a connection is turned away while another computer is served).
func serviceTXT(busy: Bool = false) -> NWTXTRecord {
    NWTXTRecord(["id": String(identity.fingerprint.prefix(16)), "port": "\(settings.config.port)", "v": "\(protocolVersion)",
                 "busy": busy ? "1" : "0"])
}

/// The code a computer that's linking has to type, in big digits. A window of its own rather than
/// a modal alert: nothing else in Sweeflow waits on it (a session may be streaming), and it's
/// taken down when the other computer stops. Main thread only.
final class CodeWindow: NSObject {
    static let shared = CodeWindow()
    private var window: NSWindow?
    private var cancelled: (() -> Void)?
    private let note = NSTextField(wrappingLabelWithString: "")
    private let button = NSButton(title: "Cancel", target: nil, action: nil)
    /// Bumped with each window, so a delayed close doesn't take down a newer one.
    private var shown = 0

    var showing: Bool { window != nil }

    /// Shows `code` for the computer called `name`. `cancelled` runs if the person here cancels.
    func show(name: String, code: String, cancelled: @escaping () -> Void) {
        close()
        // The explanation has done its job once a computer asks.
        PairingWindow.shared.close()
        shown += 1
        let title = NSTextField(wrappingLabelWithString: "Enter this code on \(name)")
        title.font = .systemFont(ofSize: 15, weight: .semibold)
        title.alignment = .center
        title.preferredMaxLayoutWidth = 300
        // Spaced out, so it reads digit by digit.
        let digits = NSTextField(labelWithString: code.map(String.init).joined(separator: " "))
        digits.font = .monospacedDigitSystemFont(ofSize: 48, weight: .semibold)
        digits.alignment = .center
        note.stringValue = "It works once, for linking \(name) to this Mac."
        note.alignment = .center
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 300
        button.title = "Cancel"
        button.target = self
        button.action = #selector(clickedButton)
        button.keyEquivalent = "\u{1b}"
        let stack = NSStackView(views: [title, digits, note, button])
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 14
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 28, bottom: 20, right: 28)
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 360, height: 220), styleMask: [.titled],
                         backing: .buffered, defer: false)
        w.title = "Link a Linux machine"
        w.contentView = stack
        w.isReleasedWhenClosed = false
        // Above other windows, as an alert would be.
        w.level = .modalPanel
        window = w
        self.cancelled = cancelled
        presentToPerson(w)
    }

    /// A line under the code (a wrong try).
    func note(_ text: String) {
        guard window != nil else { return }
        note.stringValue = text
    }

    /// Says how it ended, then goes by itself (or on OK).
    func finish(_ text: String) {
        guard window != nil else { return }
        cancelled = nil
        note.stringValue = text
        note.textColor = .labelColor
        button.title = "OK"
        button.keyEquivalent = "\r"
        let n = shown
        DispatchQueue.main.asyncAfter(deadline: .now() + 4) { [weak self] in
            if self?.shown == n { self?.close() }
        }
    }

    /// Takes the window down, without telling anyone.
    func close() {
        cancelled = nil
        guard let w = window else { return }
        window = nil
        w.close()
        BuiltInScreen.resume()
    }

    @objc private func clickedButton() {
        let cancelled = self.cancelled
        close()
        cancelled?()
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
            "Open Sweeflow on the Linux machine, choose “Link a Mac…” and pick this Mac. "
            + "This Mac then shows a code to type there.")
        how.preferredMaxLayoutWidth = 340
        let manual = NSTextField(wrappingLabelWithString: "If this Mac isn't found by itself, type its address there.")
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
        // The code, if it's up, keeps the screen lit until it's done.
        if !CodeWindow.shared.showing { BuiltInScreen.resume() }
    }
}
