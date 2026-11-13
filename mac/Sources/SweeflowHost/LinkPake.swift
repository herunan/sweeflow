import Foundation
import CryptoKit

/// The password-authenticated key exchange behind linking (PROTOCOL.md, Linking): CPace over
/// X25519, with the code this Mac shows as the password and both TLS certificates' fingerprints
/// (as this Mac saw them in the handshake) bound in. The Linux client's `pake` module
/// (linux/src/pake.rs) is the same sum, step for step, and says at length why it holds up; the
/// tests on both sides check the same vectors.
///
/// In short: both sides derive a generator point from the code and the fingerprints (Elligator 2,
/// so it's always on the curve itself and the shares say nothing about the code), each sends its
/// random scalar times that point, and each proves with a tag that it got the same key. Someone
/// who doesn't know the code tests one guess per run, and only by talking to this Mac, which
/// counts them. X25519's clamping (scalars are multiples of 8) clears any small-order part of a
/// share received, Curve25519 is twist-secure, and an all-zero key is refused.
enum LinkPake {
    static let label = Data("sweeflow-link/3".utf8)

    struct Tags: Equatable {
        let client: Data
        let host: Data
    }

    /// One side's half of a run: its scalar (secret) and its share.
    struct Share {
        let key: Curve25519.KeyAgreement.PrivateKey
        let pub: Data

        init(generator: Data, scalar: Data) throws {
            key = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: scalar)
            pub = try LinkPake.x25519(key, generator)
        }

        static func random(generator: Data) throws -> Share {
            try Share(generator: generator, scalar: Curve25519.KeyAgreement.PrivateKey().rawRepresentation)
        }
    }

    struct SmallOrder: Error {}

    /// X25519 of this scalar and any u-coordinate (CryptoKit's key agreement is exactly that).
    static func x25519(_ key: Curve25519.KeyAgreement.PrivateKey, _ u: Data) throws -> Data {
        guard u.count == 32 else { throw SmallOrder() }
        let secret = try key.sharedSecretFromKeyAgreement(with: Curve25519.KeyAgreement.PublicKey(rawRepresentation: u))
        let out = secret.withUnsafeBytes { Data($0) }
        guard out.contains(where: { $0 != 0 }) else { throw SmallOrder() }
        return out
    }

    /// The generator for this code and these two certificates.
    static func generator(code: String, client: String, host: String) -> Data {
        var r = hash([label, Data("generator".utf8), Data(code.utf8), Data(client.lowercased().utf8), Data(host.lowercased().utf8)])
        r[31] &= 0x7f
        return Field.bytes(elligator2(Field.reduce(Field.from(r))))
    }

    /// This Mac's view of a run: its share, and the computer's.
    static func hostTags(ours: Share, clientShare: Data, client: String, host: String) throws -> Tags {
        try finish(ours: ours, theirs: clientShare, client: client, host: host, clientShare: clientShare, hostShare: ours.pub)
    }

    /// The other side's view (for tests).
    static func clientTags(ours: Share, hostShare: Data, client: String, host: String) throws -> Tags {
        try finish(ours: ours, theirs: hostShare, client: client, host: host, clientShare: ours.pub, hostShare: hostShare)
    }

    private static func finish(ours: Share, theirs: Data, client: String, host: String, clientShare: Data, hostShare: Data) throws -> Tags {
        let k = try x25519(ours.key, theirs)
        let key = SymmetricKey(data: hash([label, Data("key".utf8), Data(client.lowercased().utf8), Data(host.lowercased().utf8),
                                           clientShare, hostShare, k]))
        func tag(_ role: String) -> Data { Data(HMAC<SHA256>.authenticationCode(for: label + Data(role.utf8), using: key)) }
        return Tags(client: tag("client"), host: tag("host"))
    }

    /// SHA-256 over these parts, each preceded by its length (one byte).
    private static func hash(_ parts: [Data]) -> Data {
        var h = SHA256()
        for p in parts {
            h.update(data: Data([UInt8(p.count)]))
            h.update(data: p)
        }
        return Data(h.finalize())
    }

    /// Elligator 2 onto Curve25519 (RFC 9380 §6.7.1, Z = 2), the u-coordinate only.
    static func elligator2(_ r: Field.Fe) -> Field.Fe {
        let r2 = Field.mul(r, r)
        let tv = Field.add(Field.one, Field.add(r2, r2))
        let inv = Field.pow(tv, Field.pMinus2)
        let minusA = Field.sub(Field.zero, Field.a)
        var x1 = Field.mul(minusA, inv)
        if x1 == Field.zero { x1 = minusA }
        let gx1 = Field.mul(x1, Field.add(Field.mul(Field.add(x1, Field.a), x1), Field.one))
        return Field.isSquare(gx1) ? x1 : Field.sub(minusA, x1)
    }

    /// Arithmetic mod p = 2^255 − 19 on four little-endian 64-bit limbs, always fully reduced.
    /// Only for the generator (public operations on the code's hash).
    enum Field {
        typealias Fe = [UInt64]
        static let p: Fe = [0xffff_ffff_ffff_ffed, .max, .max, 0x7fff_ffff_ffff_ffff]
        static let pMinus2: Fe = [0xffff_ffff_ffff_ffeb, .max, .max, 0x7fff_ffff_ffff_ffff]
        static let halfP: Fe = [0xffff_ffff_ffff_fff6, .max, .max, 0x3fff_ffff_ffff_ffff]
        static let zero: Fe = [0, 0, 0, 0]
        static let one: Fe = [1, 0, 0, 0]
        static let a: Fe = [486_662, 0, 0, 0]

        static func geq(_ x: Fe, _ y: Fe) -> Bool {
            for i in (0..<4).reversed() where x[i] != y[i] { return x[i] > y[i] }
            return true
        }

        static func subRaw(_ x: Fe, _ y: Fe) -> (Fe, Bool) {
            var out = zero
            var borrow = false
            for i in 0..<4 {
                let (d1, b1) = x[i].subtractingReportingOverflow(y[i])
                let (d2, b2) = d1.subtractingReportingOverflow(borrow ? 1 : 0)
                out[i] = d2
                borrow = b1 || b2
            }
            return (out, borrow)
        }

        static func addRaw(_ x: Fe, _ y: Fe) -> (Fe, Bool) {
            var out = zero
            var carry = false
            for i in 0..<4 {
                let (s1, c1) = x[i].addingReportingOverflow(y[i])
                let (s2, c2) = s1.addingReportingOverflow(carry ? 1 : 0)
                out[i] = s2
                carry = c1 || c2
            }
            return (out, carry)
        }

        static func reduce(_ x: Fe) -> Fe {
            var x = x
            while geq(x, p) { x = subRaw(x, p).0 }
            return x
        }

        static func add(_ x: Fe, _ y: Fe) -> Fe { reduce(addRaw(x, y).0) }

        static func sub(_ x: Fe, _ y: Fe) -> Fe {
            let (d, borrow) = subRaw(x, y)
            // x − y + 2^256 when it borrowed; adding p and dropping the carry gives x − y + p.
            return borrow ? addRaw(d, p).0 : d
        }

        static func mul(_ x: Fe, _ y: Fe) -> Fe {
            // The 512-bit product, limb by limb.
            var t = [UInt64](repeating: 0, count: 8)
            for i in 0..<4 {
                var carry: UInt64 = 0
                for j in 0..<4 {
                    let (hi, lo) = x[i].multipliedFullWidth(by: y[j])
                    let (s1, c1) = lo.addingReportingOverflow(t[i + j])
                    let (s2, c2) = s1.addingReportingOverflow(carry)
                    t[i + j] = s2
                    carry = hi &+ (c1 ? 1 : 0) &+ (c2 ? 1 : 0)
                }
                t[i + 4] = carry
            }
            // 2^256 ≡ 38 (mod p): fold the top half onto the bottom one.
            var r = zero
            var carry: UInt64 = 0
            for i in 0..<4 {
                let (hi, lo) = t[i + 4].multipliedFullWidth(by: 38)
                let (s1, c1) = lo.addingReportingOverflow(t[i])
                let (s2, c2) = s1.addingReportingOverflow(carry)
                r[i] = s2
                carry = hi &+ (c1 ? 1 : 0) &+ (c2 ? 1 : 0)
            }
            let (r2, c) = addRaw(r, [38 &* carry, 0, 0, 0])
            return reduce(c ? addRaw(r2, [38, 0, 0, 0]).0 : r2)
        }

        static func pow(_ x: Fe, _ e: Fe) -> Fe {
            var r = one
            for i in (0..<4).reversed() {
                for bit in (0..<64).reversed() {
                    r = mul(r, r)
                    if (e[i] >> UInt64(bit)) & 1 == 1 { r = mul(r, x) }
                }
            }
            return r
        }

        static func isSquare(_ v: Fe) -> Bool {
            let l = pow(v, halfP)
            return l == one || l == zero
        }

        static func from(_ d: Data) -> Fe {
            let b = Array(d)
            return (0..<4).map { i in (0..<8).reduce(UInt64(0)) { $0 | UInt64(b[8 * i + $1]) << UInt64(8 * $1) } }
        }

        static func bytes(_ f: Fe) -> Data {
            Data((0..<32).map { UInt8(truncatingIfNeeded: f[$0 / 8] >> UInt64(8 * ($0 % 8))) })
        }
    }

    /// Compares tags without stopping at the first difference.
    static func same(_ x: Data, _ y: Data) -> Bool {
        x.count == y.count && zip(x, y).reduce(UInt8(0)) { $0 | ($1.0 ^ $1.1) } == 0
    }
}
