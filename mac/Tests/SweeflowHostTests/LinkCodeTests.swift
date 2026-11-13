import Foundation
import XCTest
@testable import SweeflowHost

final class LinkCodeTests: XCTestCase {
    let client = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
    let host = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

    private func nonce(_ byte: UInt8) -> Data { Data(repeating: byte, count: 32) }

    /// Same inputs and codes as the Linux client's test (linux/src/pair.rs, codes_match_the_mac):
    /// the two sides must show the same code.
    func testCodesMatchLinux() {
        XCTAssertEqual(LinkCode.of(client: client, host: host, clientNonce: nonce(0x11), hostNonce: nonce(0x22)), "QAM-3TF")
        XCTAssertEqual(LinkCode.of(client: host, host: client, clientNonce: nonce(0x11), hostNonce: nonce(0x22)), "7M6-U9E")
        XCTAssertEqual(LinkCode.of(client: client, host: host, clientNonce: nonce(0x22), hostNonce: nonce(0x11)), "Q8G-DRE")
        XCTAssertEqual(LinkCode.of(client: String(repeating: "0", count: 64), host: String(repeating: "f", count: 64),
                                   clientNonce: nonce(0), hostNonce: nonce(0xff)), "KFW-AZV")
        // Fingerprints count the same in either case.
        XCTAssertEqual(LinkCode.of(client: client.uppercased(), host: host, clientNonce: nonce(0x11), hostNonce: nonce(0x22)), "QAM-3TF")
    }

    func testCodesAreTwoGroupsOfThreeWithoutLookAlikes() {
        for i in 0...255 {
            let code = Array(LinkCode.of(client: client, host: host, clientNonce: nonce(UInt8(i)), hostNonce: nonce(UInt8(255 - i))))
            XCTAssertEqual(code.count, 7)
            XCTAssertEqual(code[3], "-")
            for (n, ch) in code.enumerated() where n != 3 {
                XCTAssertTrue(ch.isASCII && (ch.isUppercase || ch.isNumber), String(code))
                XCTAssertFalse("0O1IL".contains(ch), String(code))
            }
        }
    }

    /// A computer's name goes into the clients file, a line per computer: it stays one line.
    func testAComputersNameStaysOneLine() {
        XCTAssertEqual(Pairing.shownName("desk\nffff second line"), "deskffff second line")
        XCTAssertEqual(Pairing.shownName("  desk  "), "desk")
        XCTAssertEqual(Pairing.shownName("\n"), "Linux machine")
        XCTAssertEqual(Pairing.shownName(String(repeating: "x", count: 200)).count, 60)
    }
}
