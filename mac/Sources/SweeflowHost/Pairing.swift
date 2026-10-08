import AppKit
import Network
import CryptoKit

/// Short codes for certificate fingerprints, shown to people while pairing: the first 40 bits
/// in Crockford base32, `XXXX-XXXX`. Linux checks the Mac's code against the certificate it
/// sees, so a fake Mac would need a key whose fingerprint starts the same (about 2^40 RSA keys).
enum PairingCode {
    private static let alphabet = Array("0123456789ABCDEFGHJKMNPQRSTVWXYZ")

    static func of(fingerprint hex: String) -> String {
        let h = Array(hex.lowercased().utf8.prefix(10))
        guard h.count == 10, let v = UInt64(String(decoding: h, as: UTF8.self), radix: 16) else { return "????-????" }
        let chars = (0..<8).map { alphabet[Int((v >> UInt64(35 - 5 * $0)) & 31)] }
        return String(chars[0..<4]) + "-" + String(chars[4..<8])
    }
}

/// This Mac as people know it (System Settings → General → Sharing → Computer name).
let macName = Host.current().localizedName ?? "Mac"

/// Bonjour service types: the streaming listener is always findable; the pairing one only while
/// pairing is open (so Linux lists exactly the Macs waiting to be paired).
let streamServiceType = "_sweeflow._udp"
let pairServiceType = "_sweeflow-pair._udp"

/// Pairing mode: a second QUIC listener that takes any client certificate, asks the person at
/// the Mac to allow it, and adds it to the paired clients. Open for a few minutes at most.
final class Pairing {
    static let shared = Pairing()
    static let port: UInt16 = 44121

    private let queue = DispatchQueue(label: "sweeflow.pairing")
    private var listener: NWListener?
    private var groups: [ObjectIdentifier: NWConnectionGroup] = [:]
    /// Open request streams (kept here: nothing else holds them while the person decides).
    private var streams: [ObjectIdentifier: MsgStream] = [:]
    private var timeout: DispatchWorkItem?
    /// Fingerprint of the last client certificate seen (fallback if a stream lacks TLS metadata).
    private var lastPeer: String?
    /// The stream whose computer the person at the Mac is being asked about.
    private var asking: ObjectIdentifier?

    /// Main thread: pairing opened/closed, or a computer was paired (its name).
    var onChange: ((_ open: Bool, _ paired: String?) -> Void)?

    var code: String { PairingCode.of(fingerprint: identity.fingerprint) }

    func open(for seconds: Int = 600) {
        queue.async { [self] in
            if listener == nil { start() }
            timeout?.cancel()
            let t = DispatchWorkItem { [weak self] in self?.stop(paired: nil) }
            timeout = t
            queue.asyncAfter(deadline: .now() + .seconds(seconds), execute: t)
        }
    }

    func close() { queue.async { self.stop(paired: nil) } }

    private func start() {
        let quic = NWProtocolQUIC.Options(alpn: ["sweeflow-pair/1"])
        quic.idleTimeout = 120_000
        // The default allows no client streams at all; the client opens one.
        quic.initialMaxStreamsBidirectional = 4
        quic.initialMaxStreamDataBidirectionalRemote = 64 << 10
        quic.initialMaxData = 256 << 10
        let sec = quic.securityProtocolOptions
        sec_protocol_options_set_local_identity(sec, identity.secIdentity)
        sec_protocol_options_set_peer_authentication_required(sec, true)
        // Anyone may ask; the person at the Mac decides (and sees the client's own code).
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
            l.stateUpdateHandler = { log("pairing listener: \($0)") }
            l.newConnectionGroupHandler = { [weak self] group in self?.accept(group) }
            l.start(queue: queue)
            listener = l
            log("pairing open, code \(code)")
            DispatchQueue.main.async { self.onChange?(true, nil) }
        } catch {
            log("pairing listener failed: \(error)")
        }
    }

    private func stop(paired: String?) {
        timeout?.cancel()
        timeout = nil
        guard let l = listener else { return }
        l.cancel()
        listener = nil
        groups.values.forEach { $0.cancel() }
        groups.removeAll()
        streams.removeAll()
        log(paired.map { "pairing closed: paired \($0)" } ?? "pairing closed")
        DispatchQueue.main.async { self.onChange?(false, paired) }
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
            // That computer gave up: don't leave the question up for nobody.
            if self.asking == key {
                self.asking = nil
                DispatchQueue.main.async { PairingWindow.shared.cancelAsk() }
            }
        }
        stream.onMessage = { [weak self, weak stream] m in
            guard let self, let stream else { return }
            guard m.type == .pairRequest else { return log("pairing: unexpected message \(m.type)") }
            var m = m
            guard let version = try? m.u16(), let name = try? m.str() else { return log("pairing: malformed request") }
            guard let fp = self.peerFingerprint(conn) ?? self.lastPeer else {
                log("pairing: no client certificate")
                return self.reply(stream, ok: false)
            }
            log("pairing request from \(name) (protocol \(version), code \(PairingCode.of(fingerprint: fp)))")
            guard self.asking == nil else { return self.reply(stream, ok: false) }
            self.asking = key
            DispatchQueue.main.async {
                PairingWindow.shared.ask(name: name, code: PairingCode.of(fingerprint: fp)) { allow in
                    self.queue.async {
                        guard self.asking == key else { return }   // gone meanwhile
                        self.asking = nil
                        if allow { Identity.addClient(fp, name: name) }
                        self.reply(stream, ok: allow)
                        // Let the reply go out before the listener closes.
                        if allow { self.queue.asyncAfter(deadline: .now() + 1) { self.stop(paired: name) } }
                    }
                }
            }
        }
        stream.start()
    }

    private func reply(_ stream: MsgStream, ok: Bool) {
        var w = MsgWriter(.pairResult)
        w.u8(ok ? 1 : 0)
        w.str(macName)
        w.u16(settings.config.port)
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
}

/// TXT record for both services: which Mac this is (stable across renames and IP changes),
/// the streaming port and the protocol version.
func serviceTXT() -> NWTXTRecord {
    NWTXTRecord(["id": String(identity.fingerprint.prefix(16)), "port": "\(settings.config.port)", "v": "\(protocolVersion)"])
}

/// A small window with the code to type on Linux, open while pairing is.
final class PairingWindow: NSObject, NSWindowDelegate {
    static let shared = PairingWindow()
    private var window: NSWindow?
    private let status = NSTextField(labelWithString: "")

    func show() {
        Pairing.shared.onChange = { [weak self] open, paired in self?.update(open: open, paired: paired) }
        Pairing.shared.open()
        if window == nil { build() }
        status.stringValue = "Waiting for Linux…"
        BuiltInScreen.pause()
        NSApp.activate()
        place()
        window?.makeKeyAndOrderFront(nil)
    }

    /// On the screen a person at the Mac sees: the built-in one, else any but Sweeflow's own
    /// virtual display (nobody sees that, and Sweeflow's windows aren't streamed).
    private func place() {
        guard let w = window else { return }
        let virtual = host.display.displayID
        let screen = BuiltInScreen.screen ?? NSScreen.screens.first {
            ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID) != virtual
        } ?? NSScreen.main
        guard let f = screen?.visibleFrame else { return w.center() }
        w.setFrameOrigin(NSPoint(x: f.midX - w.frame.width / 2, y: f.midY - w.frame.height / 2))
    }

    /// Asks the person at the Mac to allow a computer, on the pairing window.
    func ask(name: String, code: String, done: @escaping (Bool) -> Void) {
        if window == nil || window?.isVisible != true { show() }
        guard let w = window else { return done(false) }
        NSApp.activate()
        let a = NSAlert()
        a.messageText = "Allow “\(name)” to use this Mac?"
        a.informativeText = "\(name) will show this Mac's apps on its desktop and control them.\n\n"
            + "Check that \(name) shows the code \(code)."
        a.addButton(withTitle: "Allow")
        a.addButton(withTitle: "Don't Allow")
        a.beginSheetModal(for: w) { done($0 == .alertFirstButtonReturn) }
    }

    func cancelAsk() {
        guard let w = window, let sheet = w.attachedSheet else { return }
        w.endSheet(sheet, returnCode: .abort)
        status.stringValue = "That computer stopped waiting. Waiting for Linux…"
    }

    private func build() {
        let title = NSTextField(labelWithString: "Pair with Linux")
        title.font = .systemFont(ofSize: 17, weight: .semibold)
        let how = NSTextField(wrappingLabelWithString:
            "On your Linux computer, open Sweeflow and choose “Add a Mac…”. Pick “\(macName)” and type this code:")
        let code = NSTextField(labelWithString: Pairing.shared.code)
        code.font = .monospacedSystemFont(ofSize: 34, weight: .medium)
        code.isSelectable = true
        status.textColor = .secondaryLabelColor
        let cancel = NSButton(title: "Cancel", target: self, action: #selector(cancel))
        cancel.keyEquivalent = "\u{1b}"
        let stack = NSStackView(views: [title, how, code, status, cancel])
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 14
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 28, bottom: 20, right: 28)
        how.preferredMaxLayoutWidth = 320
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 380, height: 260), styleMask: [.titled, .closable],
                         backing: .buffered, defer: false)
        w.title = "Sweeflow"
        w.contentView = stack
        w.isReleasedWhenClosed = false
        w.delegate = self
        window = w
    }

    private func update(open: Bool, paired: String?) {
        if let paired {
            status.stringValue = "Paired with \(paired)."
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in self?.window?.close() }
        } else if !open {
            window?.close()
        }
    }

    @objc private func cancel() { window?.close() }

    func windowWillClose(_ notification: Notification) {
        Pairing.shared.close()
        BuiltInScreen.resume()
    }
}
