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
    /// Linking isn't open on this Mac: its Link window isn't open (or setup isn't finished). The
    /// computer waits and asks again.
    case closed = 11
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
    /// Link requests are taken only while the Link window is open, which only opens once every
    /// permission is on: no code pops up uninvited, and nothing links before this Mac can stream.
    private var linkingOpen = false

    /// The Link window opened or closed (main thread): this Mac is findable for linking only
    /// while it's open (the service is announced then, and taken down after).
    func setOpen(_ open: Bool) {
        queue.async {
            self.linkingOpen = open
            self.announce()
        }
    }

    /// The linking service on the network, or not, as the Link window is.
    private func announce() {
        guard let l = listener else { return }
        l.service = linkingOpen ? NWListener.Service(name: macName, type: pairServiceType, domain: nil, txtRecord: serviceTXT()) : nil
        log("pairing: \(linkingOpen ? "findable for linking" : "not findable for linking")")
    }

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
            // Announced only while the Link window is open (announce).
            if linkingOpen {
                l.service = NWListener.Service(name: macName, type: pairServiceType, domain: nil, txtRecord: serviceTXT())
            }
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
        guard let fp = (Server.peerFingerprint(conn) ?? lastPeer)?.lowercased() else {
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
        guard linkingOpen else {
            log("pairing: \(name) asks, but linking isn't open here")
            return reply(stream, .closed)
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
            DispatchQueue.main.async {
                CodeWindow.shared.finish("Linked to \(a.name).")
                PairingWindow.shared.linked(a.name)
            }
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
        guard let fp = Server.peerFingerprint(conn)?.lowercased() else { return reply(stream, .busy) }
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
        guard let fp = Server.peerFingerprint(conn)?.lowercased() else { return reply(stream, .busy) }
        if Identity.pairedClients().contains(where: { $0.fingerprint == fp }) {
            return reply(stream, .linked, start: Identity.startWanted().contains(fp))
        }
        // Told now: no need to tell it when it next connects.
        if Identity.forgottenClients().contains(fp) { Identity.setForgotten(fp, false) }
        reply(stream, .notLinked)
    }

    private func reply(_ stream: MsgStream, _ status: PairStatus, tag: Data = Data(), start: Bool = false) {
        var w = MsgWriter(.pairResult)
        w.u8(status.rawValue)
        w.str(macName)
        w.u16(settings.config.port)
        w.u16(Pairing.version)
        w.bytes(tag)
        // Streaming to a computer now: a linked computer that can't see this Mac's network
        // announcement (a virtual machine's network, a network that drops it) asks instead.
        w.u8(Server.serving ? 1 : 0)
        w.u8(ScreenLock.isLocked ? 1 : 0)
        // Start on this Mac asked for the computer asking.
        w.u8(start ? 1 : 0)
        stream.send(w)
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
                 "busy": busy ? "1" : "0", "locked": ScreenLock.isLocked ? "1" : "0",
                 // Start on this Mac, for that computer (the start of its fingerprint): it starts.
                 "start": Identity.startWanted().first.map { String($0.prefix(16)) } ?? ""])
}

/// The code a computer that's linking has to type: shown in the Link window, under its steps.
/// Linking only happens while that window is open, so the code always has it to appear in (one
/// window, not a second one popping up). Main thread only.
final class CodeWindow {
    static let shared = CodeWindow()
    var showing: Bool { PairingWindow.shared.codeShowing }

    /// Shows `code` for the computer called `name`. `cancelled` runs if the person here cancels.
    func show(name: String, code: String, cancelled: @escaping () -> Void) {
        PairingWindow.shared.showCode(name: name, code: code, cancelled: cancelled)
    }
    /// A line under the code (a wrong try).
    func note(_ text: String) { PairingWindow.shared.codeNote(text) }
    /// Says how it ended, then the code goes by itself.
    func finish(_ text: String) { PairingWindow.shared.codeFinished(text) }
    /// Takes the code down, without telling anyone.
    func close() { PairingWindow.shared.hideCode() }
}

/// What "Link a Linux machine…" opens, and where setup ends: while it's open this Mac takes link
/// requests (`Pairing.setOpen`). Linking starts on Linux, so it says how, with this Mac's name and
/// address for when it isn't found by itself. Only opens once every permission is on; before that
/// the setup window opens instead. Main thread only.
final class PairingWindow: NSObject, NSWindowDelegate {
    static let shared = PairingWindow()
    private var window: NSWindow?
    private var status: NSTextField?
    private var spinner: NSProgressIndicator?
    private var button: NSButton?
    private var stack: NSStackView?
    // The code, under the steps while one is up.
    private var codeBox: NSView?
    private var codeTitle: NSTextField?
    private var codeDigits: NSTextField?
    private var codeNoteLabel: NSTextField?
    private var cancelCode: (() -> Void)?
    private(set) var codeShowing = false
    /// Drawn into a picture (`--render-link`): nothing of the running app is there to bring it forward.
    private var previewing = false
    /// Bumped with each code, so a delayed hide doesn't take down a newer one.
    private var codesShown = 0

    func show() {
        guard Onboarding.permissionsReady else { return Onboarding.shared.show() }
        // Built each time: this Mac's addresses change with the network.
        window?.close()
        let w = build()
        window = w
        Pairing.shared.setOpen(true)
        presentToPerson(w)
    }

    func close() { window?.close() }

    /// A computer asked to link: its code, in the window (opened if it somehow isn't).
    func showCode(name: String, code: String, cancelled: @escaping () -> Void) {
        if window == nil { show() }
        guard let w = window else { return }
        codesShown += 1
        codeShowing = true
        cancelCode = cancelled
        codeTitle?.stringValue = "Enter this code on \(name)"
        // Spaced out, so it reads digit by digit.
        codeDigits?.stringValue = code.map(String.init).joined(separator: " ")
        codeNoteLabel?.stringValue = ""
        codeNoteLabel?.isHidden = true
        codeBox?.isHidden = false
        status?.stringValue = "Waiting for the code on \(name)."
        button?.title = "Cancel"
        fit()
        if !previewing { presentToPerson(w) }
    }

    func codeNote(_ text: String) {
        codeNoteLabel?.stringValue = text
        codeNoteLabel?.textColor = .secondaryLabelColor
        codeNoteLabel?.isHidden = text.isEmpty
        fit()
    }

    func codeFinished(_ text: String) {
        guard codeShowing else { return }
        cancelCode = nil
        codeNote(text)
        codeNoteLabel?.textColor = .labelColor
        let n = codesShown
        DispatchQueue.main.asyncAfter(deadline: .now() + 4) { [weak self] in
            if self?.codesShown == n { self?.hideCode() }
        }
    }

    func hideCode() {
        cancelCode = nil
        guard codeShowing else { return }
        codeShowing = false
        codeBox?.isHidden = true
        if button?.title == "Cancel" {
            button?.title = "Close"
            status?.stringValue = "Waiting for your Linux machine."
        }
        fit()
    }

    /// The window as tall as what it shows (the code comes and goes).
    private func fit() {
        guard let w = window, let stack else { return }
        stack.layoutSubtreeIfNeeded()
        let h = stack.fittingSize.height
        guard abs(w.contentLayoutRect.height - h) > 0.5 else { return }
        var f = w.frame
        let newFrame = w.frameRect(forContentRect: NSRect(x: 0, y: 0, width: Self.width, height: h))
        f.origin.y += f.height - newFrame.height
        f.size = newFrame.size
        w.setFrame(f, display: true, animate: w.isVisible)
    }

    /// A computer was just linked: says so, then goes.
    func linked(_ name: String) {
        guard window != nil else { return }
        spinner?.stopAnimation(nil)
        spinner?.isHidden = true
        status?.stringValue = "Linked with \(name). Your Mac's apps show up there now."
        cancelCode = nil
        status?.textColor = .labelColor
        button?.title = "Done"
        Pairing.shared.setOpen(false)
        DispatchQueue.main.asyncAfter(deadline: .now() + 6) { [weak self] in self?.close() }
    }

    static let width: CGFloat = 520

    private func build() -> NSWindow {
        let inner = Self.width - 56
        let icon = NSImageView(image: NSApp.applicationIconImage)
        icon.translatesAutoresizingMaskIntoConstraints = false
        icon.widthAnchor.constraint(equalToConstant: 64).isActive = true
        icon.heightAnchor.constraint(equalToConstant: 64).isActive = true
        let title = NSTextField(labelWithString: "Link a Linux machine")
        title.font = .systemFont(ofSize: 22, weight: .semibold)
        let subtitle = NSTextField(labelWithString: "This Mac can be found while this window is open.")
        subtitle.font = .systemFont(ofSize: 13)
        subtitle.textColor = .secondaryLabelColor
        let words = NSStackView(views: [title, subtitle])
        words.orientation = .vertical
        words.alignment = .leading
        words.spacing = 3
        let header = NSStackView(views: [icon, words])
        header.spacing = 14
        header.alignment = .centerY

        let steps: [(String, String)] = [
            ("On your Linux machine, open Sweeflow.", ""),
            ("Choose this Mac.", "It's listed as \(macName)."),
            ("Type the code that shows here.", "It appears on this Mac by itself."),
        ]
        var rows: [NSView] = []
        for (i, step) in steps.enumerated() {
            if i > 0 { rows.append(Self.line(width: inner)) }
            rows.append(Self.stepRow(number: i + 1, title: step.0, detail: step.1, width: inner))
        }
        let list = NSStackView(views: rows)
        list.orientation = .vertical
        list.alignment = .leading
        list.spacing = 0
        list.translatesAutoresizingMaskIntoConstraints = false
        let group = GroupView()
        group.translatesAutoresizingMaskIntoConstraints = false
        group.addSubview(list)
        NSLayoutConstraint.activate([
            group.widthAnchor.constraint(equalToConstant: inner),
            list.topAnchor.constraint(equalTo: group.topAnchor),
            list.bottomAnchor.constraint(equalTo: group.bottomAnchor),
            list.leadingAnchor.constraint(equalTo: group.leadingAnchor),
            list.trailingAnchor.constraint(equalTo: group.trailingAnchor),
        ])

        // The code, once Linux asks: under the steps, hidden until then.
        let codeTitle = NSTextField(wrappingLabelWithString: "")
        codeTitle.font = .systemFont(ofSize: 15, weight: .semibold)
        codeTitle.alignment = .center
        codeTitle.preferredMaxLayoutWidth = inner - 40
        let digits = NSTextField(labelWithString: "")
        digits.font = .monospacedDigitSystemFont(ofSize: 44, weight: .semibold)
        digits.textColor = .white
        digits.alignment = .center
        digits.translatesAutoresizingMaskIntoConstraints = false
        // White digits on Sweeflow's deeper violet: clear in light and dark alike.
        let plate = NSView()
        plate.wantsLayer = true
        plate.layer?.cornerRadius = 14
        plate.layer?.backgroundColor = NSColor(srgbRed: 0x7C / 255, green: 0x3A / 255, blue: 0xED / 255, alpha: 1).cgColor
        plate.translatesAutoresizingMaskIntoConstraints = false
        plate.addSubview(digits)
        NSLayoutConstraint.activate([
            digits.leadingAnchor.constraint(equalTo: plate.leadingAnchor, constant: 30),
            digits.trailingAnchor.constraint(equalTo: plate.trailingAnchor, constant: -30),
            digits.topAnchor.constraint(equalTo: plate.topAnchor, constant: 10),
            digits.bottomAnchor.constraint(equalTo: plate.bottomAnchor, constant: -10),
        ])
        let codeNote = NSTextField(wrappingLabelWithString: "")
        codeNote.font = .systemFont(ofSize: 12)
        codeNote.alignment = .center
        codeNote.preferredMaxLayoutWidth = inner - 40
        codeNote.isHidden = true
        let codeStack = NSStackView(views: [codeTitle, plate, codeNote])
        codeStack.orientation = .vertical
        codeStack.alignment = .centerX
        codeStack.spacing = 12
        codeStack.translatesAutoresizingMaskIntoConstraints = false
        let codeBox = GroupView()
        codeBox.translatesAutoresizingMaskIntoConstraints = false
        codeBox.addSubview(codeStack)
        NSLayoutConstraint.activate([
            codeBox.widthAnchor.constraint(equalToConstant: inner),
            codeStack.topAnchor.constraint(equalTo: codeBox.topAnchor, constant: 18),
            codeStack.bottomAnchor.constraint(equalTo: codeBox.bottomAnchor, constant: -18),
            codeStack.centerXAnchor.constraint(equalTo: codeBox.centerXAnchor),
        ])
        codeBox.isHidden = true
        self.codeBox = codeBox
        self.codeTitle = codeTitle
        self.codeDigits = digits
        self.codeNoteLabel = codeNote

        // Typed by hand on Linux when this Mac isn't found by itself; Tailscale's address works
        // from anywhere the two are on the same tailnet.
        let addresses = Self.addresses()
        let local = addresses.filter { !Self.isTailscale($0) }, tailnet = addresses.filter(Self.isTailscale)
        let manual = Self.addressLine(local.isEmpty
            ? ("This Mac isn't on a local network right now.", [])
            : ("Not found automatically? Type this address on Linux: ", local), width: inner)
        let tailscale = Self.addressLine(tailnet.isEmpty
            ? ("Works with Tailscale too.", [])
            : ("Works with Tailscale too: ", tailnet), width: inner)

        let spin = NSProgressIndicator()
        spin.style = .spinning
        spin.controlSize = .small
        spin.startAnimation(nil)
        let status = NSTextField(wrappingLabelWithString: "Waiting for your Linux machine.")
        status.font = .systemFont(ofSize: 12)
        status.textColor = .secondaryLabelColor
        status.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let button = NSButton(title: "Close", target: self, action: #selector(clickedClose))
        button.bezelStyle = .push
        button.keyEquivalent = "\u{1b}"
        let gap = NSView()
        gap.setContentHuggingPriority(.defaultLow, for: .horizontal)
        let footer = NSStackView(views: [spin, status, gap, button])
        footer.alignment = .centerY
        footer.spacing = 8
        footer.translatesAutoresizingMaskIntoConstraints = false
        footer.widthAnchor.constraint(equalToConstant: inner).isActive = true
        self.status = status
        self.spinner = spin
        self.button = button

        let stack = NSStackView(views: [header, group, codeBox, manual, tailscale, Self.line(width: inner, indent: 0), footer])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.setCustomSpacing(22, after: header)
        stack.setCustomSpacing(14, after: group)
        stack.setCustomSpacing(14, after: codeBox)
        stack.setCustomSpacing(6, after: manual)
        stack.setCustomSpacing(18, after: tailscale)
        self.stack = stack
        stack.edgeInsets = NSEdgeInsets(top: 40, left: 28, bottom: 20, right: 28)

        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: Self.width, height: 400),
                         styleMask: [.titled, .closable, .fullSizeContentView], backing: .buffered, defer: false)
        w.title = "Link a Linux machine"
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isMovableByWindowBackground = true
        w.contentView = stack
        w.isReleasedWhenClosed = false
        w.delegate = self
        stack.layoutSubtreeIfNeeded()
        w.setContentSize(NSSize(width: Self.width, height: stack.fittingSize.height))
        return w
    }

    /// A numbered step: the number in a violet circle, what to do, and a second line if any.
    private static func stepRow(number: Int, title: String, detail: String, width: CGFloat) -> NSView {
        let badge = NSTextField(labelWithString: "\(number)")
        badge.font = .systemFont(ofSize: 13, weight: .semibold)
        badge.textColor = .white
        badge.alignment = .center
        let circle = NSView()
        circle.wantsLayer = true
        circle.layer?.cornerRadius = 12
        circle.layer?.backgroundColor = NSColor.sweeflowViolet.cgColor
        circle.translatesAutoresizingMaskIntoConstraints = false
        badge.translatesAutoresizingMaskIntoConstraints = false
        circle.addSubview(badge)
        NSLayoutConstraint.activate([
            circle.widthAnchor.constraint(equalToConstant: 24), circle.heightAnchor.constraint(equalToConstant: 24),
            badge.centerXAnchor.constraint(equalTo: circle.centerXAnchor), badge.centerYAnchor.constraint(equalTo: circle.centerYAnchor),
        ])
        let t = NSTextField(labelWithString: title)
        t.font = .systemFont(ofSize: 13, weight: .semibold)
        var texts: [NSView] = [t]
        if !detail.isEmpty {
            let d = NSTextField(wrappingLabelWithString: detail)
            d.font = .systemFont(ofSize: 12)
            d.textColor = .secondaryLabelColor
            d.preferredMaxLayoutWidth = width - 80
            texts.append(d)
        }
        let text = NSStackView(views: texts)
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 2
        let row = NSStackView(views: [circle, text])
        row.alignment = .centerY
        row.spacing = 14
        row.translatesAutoresizingMaskIntoConstraints = false
        let holder = NSView()
        holder.translatesAutoresizingMaskIntoConstraints = false
        holder.addSubview(row)
        NSLayoutConstraint.activate([
            holder.widthAnchor.constraint(equalToConstant: width),
            row.topAnchor.constraint(equalTo: holder.topAnchor, constant: 12),
            row.bottomAnchor.constraint(equalTo: holder.bottomAnchor, constant: -12),
            row.leadingAnchor.constraint(equalTo: holder.leadingAnchor, constant: 18),
            row.trailingAnchor.constraint(lessThanOrEqualTo: holder.trailingAnchor, constant: -16),
        ])
        return holder
    }

    private static func line(width: CGFloat, indent: CGFloat = 56) -> NSView {
        let line = NSBox()
        line.boxType = .separator
        line.translatesAutoresizingMaskIntoConstraints = false
        let holder = NSView()
        holder.translatesAutoresizingMaskIntoConstraints = false
        holder.addSubview(line)
        NSLayoutConstraint.activate([
            holder.widthAnchor.constraint(equalToConstant: width),
            holder.heightAnchor.constraint(equalToConstant: 1),
            line.leadingAnchor.constraint(equalTo: holder.leadingAnchor, constant: indent),
            line.trailingAnchor.constraint(equalTo: holder.trailingAnchor),
            line.centerYAnchor.constraint(equalTo: holder.centerYAnchor),
        ])
        return holder
    }

    /// This Mac's IPv4 addresses another computer can use: not its own loopback, nor the ones
    /// made up on a network that hands none out.
    private static func addresses() -> [String] {
        // The interfaces' own (getifaddrs): Host.current() can wait on name lookups, here on
        // the main thread. Not the .local name the list ends with.
        Session.ownAddresses().filter { !$0.hasSuffix(".local") }
    }

    /// A line of explanation with the addresses in it, in a fixed-width face (selectable, to copy).
    private static func addressLine(_ parts: (String, [String]), width: CGFloat) -> NSTextField {
        let small = NSFont.systemFont(ofSize: 12)
        let text = NSMutableAttributedString(string: parts.0, attributes: [.font: small, .foregroundColor: NSColor.secondaryLabelColor])
        for (i, a) in parts.1.enumerated() {
            if i > 0 { text.append(NSAttributedString(string: " or ", attributes: [.font: small, .foregroundColor: NSColor.secondaryLabelColor])) }
            text.append(NSAttributedString(string: a, attributes: [.font: NSFont.monospacedSystemFont(ofSize: 12, weight: .semibold),
                                                                     .foregroundColor: NSColor.labelColor]))
        }
        let label = NSTextField(wrappingLabelWithString: "")
        label.attributedStringValue = text
        label.isSelectable = true
        label.preferredMaxLayoutWidth = width
        return label
    }

    /// A Tailscale address: 100.64.0.0/10, the range Tailscale hands out.
    private static func isTailscale(_ address: String) -> Bool {
        let parts = address.split(separator: ".").compactMap { Int($0) }
        return parts.count == 4 && parts[0] == 100 && (64...127).contains(parts[1])
    }

    /// Close; while a code is up, Cancel (the window stays, for another try).
    @objc private func clickedClose() {
        if codeShowing {
            let cancelled = cancelCode
            hideCode()
            cancelled?()
        } else {
            window?.close()
        }
    }

    func windowWillClose(_ notification: Notification) {
        // Closed with a code up: that linking stops.
        let cancelled = codeShowing ? cancelCode : nil
        codeShowing = false
        cancelCode = nil
        cancelled?()
        window = nil
        Pairing.shared.setOpen(false)
        BuiltInScreen.resume()
    }

    /// `SweeflowHost --render-link FILE.png`: draws the window into a picture, then quits.
    static func render(to path: String, code: String? = nil) -> Never {
        let p = PairingWindow()
        let w = p.build()
        p.window = w
        p.previewing = true
        w.alphaValue = 0
        if let code { p.showCode(name: "my-pc", code: code) {} }
        w.setFrameOrigin(NSPoint(x: -20000, y: -20000))
        w.orderFrontRegardless()
        RunLoop.main.run(until: Date().addingTimeInterval(1))
        if let view = w.contentView, let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) {
            view.cacheDisplay(in: view.bounds, to: rep)
            try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
        }
        exit(0)
    }
}

extension NSColor {
    /// Sweeflow's violet (the app icon's, the site's), lighter on dark backgrounds.
    static let sweeflowViolet = NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0xA7 / 255, green: 0x8B / 255, blue: 0xFA / 255, alpha: 1)
            : NSColor(srgbRed: 0x7C / 255, green: 0x3A / 255, blue: 0xED / 255, alpha: 1)
    }
}
