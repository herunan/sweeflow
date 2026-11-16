import Foundation
import XCTest
@testable import SweeflowHost

final class LinkPakeTests: XCTestCase {
    let client = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
    let host = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

    private func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

    /// Same inputs and outputs as the Linux client's test (linux/src/pake.rs, vectors_match_the_mac):
    /// the two sides must agree byte for byte.
    func testVectorsMatchLinux() throws {
        let g = LinkPake.generator(code: "4821", client: client, host: host)
        XCTAssertEqual(hex(g), "f38495f3d09d4c6de9368bd1637adfc8d604414c2462299affc4dbc3a9da9164")
        let c = try LinkPake.Share(generator: g, scalar: Data(repeating: 0x11, count: 32))
        let h = try LinkPake.Share(generator: g, scalar: Data(repeating: 0x22, count: 32))
        XCTAssertEqual(hex(c.pub), "d410d7e9ee08dc0385033158aa11ced69db475ce190e7a927d1af8b9791e457d")
        XCTAssertEqual(hex(h.pub), "0d2f93760c8fe9169de24088de63a4e3ea94fa7c6e8b6a2b2dc0f4e70b345a27")
        let t = try LinkPake.hostTags(ours: h, clientShare: c.pub, client: client, host: host)
        XCTAssertEqual(hex(t.client), "6b71d09d0bae6b969a1e5538e41e86f7c26fe534f167a16d58333b6791a51de7")
        XCTAssertEqual(hex(t.host), "0b627cbbf9957b7d2a350324ceb4d185fd6837222f6b5cf0812f54f98f452c47")
        XCTAssertEqual(hex(LinkPake.generator(code: "0000", client: String(repeating: "0", count: 64), host: String(repeating: "F", count: 64))),
                       "ab0bed9d25f73ed1252cd59249bbdebfb7e3ddaadd9f6bb1d2f07970dfb89a4f")
    }

    func testTheSameCodeGivesBothSidesTheSameTags() throws {
        let g = LinkPake.generator(code: "1234", client: client, host: host)
        let c = try LinkPake.Share.random(generator: g)
        let h = try LinkPake.Share.random(generator: g)
        let a = try LinkPake.clientTags(ours: c, hostShare: h.pub, client: client, host: host)
        let b = try LinkPake.hostTags(ours: h, clientShare: c.pub, client: client, host: host)
        XCTAssertEqual(a, b)
        XCTAssertNotEqual(a.client, a.host)
    }

    func testAWrongCodeOrAnotherCertificateGivesOtherTags() throws {
        let c = try LinkPake.Share.random(generator: LinkPake.generator(code: "1234", client: client, host: host))
        let h = try LinkPake.Share.random(generator: LinkPake.generator(code: "1235", client: client, host: host))
        XCTAssertNotEqual(try LinkPake.clientTags(ours: c, hostShare: h.pub, client: client, host: host).client,
                          try LinkPake.hostTags(ours: h, clientShare: c.pub, client: client, host: host).client)
        // Someone in between: this Mac sees its certificate where Linux's should be.
        let other = String(repeating: "e", count: 64)
        let c2 = try LinkPake.Share.random(generator: LinkPake.generator(code: "1234", client: client, host: host))
        let h2 = try LinkPake.Share.random(generator: LinkPake.generator(code: "1234", client: other, host: host))
        XCTAssertNotEqual(try LinkPake.clientTags(ours: c2, hostShare: h2.pub, client: client, host: host).client,
                          try LinkPake.hostTags(ours: h2, clientShare: c2.pub, client: other, host: host).client)
    }

    func testASmallOrderShareIsRefused() throws {
        let h = try LinkPake.Share.random(generator: LinkPake.generator(code: "1234", client: client, host: host))
        XCTAssertThrowsError(try LinkPake.hostTags(ours: h, clientShare: Data(count: 32), client: client, host: host))
        var one = Data(count: 32)
        one[0] = 1
        XCTAssertThrowsError(try LinkPake.hostTags(ours: h, clientShare: one, client: client, host: host))
    }

    func testFieldArithmetic() {
        typealias F = LinkPake.Field
        let minusOne = F.sub(F.zero, F.one)
        XCTAssertEqual(F.mul(minusOne, minusOne), F.one)
        XCTAssertEqual(F.add(minusOne, F.one), F.zero)
        let x = F.reduce(F.from(Data((0..<32).map { UInt8($0 * 7 + 3) })))
        XCTAssertEqual(F.mul(x, F.pow(x, F.pMinus2)), F.one)
        XCTAssertTrue(F.isSquare(F.mul(x, x)))
        XCTAssertFalse(F.isSquare([2, 0, 0, 0]))
        // Every generator is on the curve itself: x³ + A·x² + x is a square.
        for i in 0..<50 {
            let u = F.from(LinkPake.generator(code: String(format: "%04d", i), client: client, host: host))
            XCTAssertTrue(F.isSquare(F.mul(u, F.add(F.mul(F.add(u, F.a), u), F.one))))
        }
    }

    func testCodesAreFourDigits() {
        for _ in 0..<100 {
            let c = LinkCode.random()
            XCTAssertEqual(c.count, 4)
            XCTAssertTrue(c.allSatisfy(\.isNumber))
        }
    }

    func testWrongCodesInARowWaitLongerAndLonger() {
        XCTAssertEqual(Pairing.cooldown(wrongInARow: 2), 0)
        XCTAssertEqual(Pairing.cooldown(wrongInARow: 3), 30)
        XCTAssertEqual(Pairing.cooldown(wrongInARow: 6), 60)
        XCTAssertEqual(Pairing.cooldown(wrongInARow: 300), 3600)
    }

    /// A computer's name goes into the clients file, a line per computer: it stays one line.
    func testAComputersNameStaysOneLine() {
        XCTAssertEqual(Pairing.shownName("desk\nffff second line"), "deskffff second line")
        XCTAssertEqual(Pairing.shownName("  desk  "), "desk")
        XCTAssertEqual(Pairing.shownName("\n"), "Linux machine")
        XCTAssertEqual(Pairing.shownName(String(repeating: "x", count: 200)).count, 60)
    }
}
