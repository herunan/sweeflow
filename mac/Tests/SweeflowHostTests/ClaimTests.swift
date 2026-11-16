import XCTest
@testable import SweeflowHost

/// Who gets the session when a second client says hello while one is being served.
final class ClaimTests: XCTestCase {
    func testAnotherComputerIsTurnedAway() {
        // However it connects (Start, login, a retry): only Stop on the one served frees the Mac.
        XCTAssertEqual(Claim.decide(sameComputer: false, heldSilentFor: 0), .busy)
        XCTAssertEqual(Claim.decide(sameComputer: false, heldSilentFor: Claim.quietFor - 0.1), .busy)
    }

    func testTheSameComputerReconnectingReplacesItsOldConnection() {
        XCTAssertEqual(Claim.decide(sameComputer: true, heldSilentFor: 0), .replace)
    }

    func testAHeldSessionGoneQuietCountsAsGone() {
        XCTAssertEqual(Claim.decide(sameComputer: false, heldSilentFor: Claim.quietFor), .replace)
    }
}
