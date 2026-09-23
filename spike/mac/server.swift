// QUIC interop spike: Network.framework server <-> quinn client.
// Accepts a QUIC connection group, echoes one reliable stream, and sends datagrams.
import Foundation
import Network
import Security

func loadIdentity(p12Path: String, password: String) -> sec_identity_t {
    let data = try! Data(contentsOf: URL(fileURLWithPath: p12Path)) as CFData
    var items: CFArray?
    let status = SecPKCS12Import(data, [kSecImportExportPassphrase as String: password, kSecImportToMemoryOnly as String: true] as CFDictionary, &items)
    guard status == errSecSuccess, let arr = items as? [[String: Any]], let first = arr.first,
          let ident = first[kSecImportItemIdentity as String] else {
        fatalError("PKCS12 import failed: \(status)")
    }
    return sec_identity_create(ident as! SecIdentity)!
}

let identity = loadIdentity(p12Path: CommandLine.arguments[1], password: "spike")

let quic = NWProtocolQUIC.Options(alpn: ["sweeflow/0"])
quic.isDatagram = true
quic.maxDatagramFrameSize = 65535
quic.idleTimeout = 30_000
sec_protocol_options_set_local_identity(quic.securityProtocolOptions, identity)
let params = NWParameters(quic: quic)

let listener = try! NWListener(using: params, on: 44120)
listener.stateUpdateHandler = { print("listener:", $0) }

var groups: [NWConnectionGroup] = []

listener.newConnectionGroupHandler = { group in
    print("new connection group")
    groups.append(group)
    group.newConnectionHandler = { conn in
        print("new stream from client")
        conn.stateUpdateHandler = { print("stream:", $0) }
        conn.start(queue: .main)
        func recv() {
            conn.receive(minimumIncompleteLength: 1, maximumLength: 65536) { data, _, done, err in
                if let data { print("stream got \(data.count)B: \(String(decoding: data, as: UTF8.self))")
                    conn.send(content: Data("echo:".utf8) + data, completion: .contentProcessed { _ in }) }
                if err == nil && !done { recv() }
            }
        }
        recv()
    }
    group.stateUpdateHandler = { state in
        print("group:", state)
        if case .ready = state {
            // Open a datagram flow on the same QUIC connection and blast some datagrams.
            let dgOpts = NWProtocolQUIC.Options(alpn: ["sweeflow/0"])
            dgOpts.isDatagram = true
            guard let dg = NWConnection(from: group, using: dgOpts) else {
                print("could not create datagram flow"); return
            }
            dg.stateUpdateHandler = { s in
                print("datagram flow:", s)
                if case .ready = s {
                    for i in 0..<100 {
                        var payload = Data(count: 1200)
                        payload[0] = UInt8(i)
                        dg.send(content: payload, completion: .contentProcessed { e in if let e { print("dg err", e) } })
                    }
                    print("sent 100 datagrams")
                }
            }
            dg.start(queue: .main)
            func recvDg() {
                dg.receiveMessage { data, _, _, err in
                    if let data { print("got client datagram \(data.count)B") }
                    if err == nil { recvDg() }
                }
            }
            recvDg()
        }
    }
    group.start(queue: .main)
}
listener.start(queue: .main)
dispatchMain()
