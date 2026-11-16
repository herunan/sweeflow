import XCTest
@testable import SweeflowHost

final class LidSleepRuleTests: XCTestCase {
    /// The rule allows exactly the two pmset calls Wakefulness makes, for that user only.
    func testRuleAllowsOnlyTheTwoPmsetCalls() {
        XCTAssertEqual(LidSleepRule.rule(user: "sam"),
                       "sam ALL=(root) NOPASSWD: /usr/bin/pmset -a disablesleep 1, /usr/bin/pmset -a disablesleep 0")
    }

    /// Checked by visudo before it's installed, root-owned and read-only; no double quotes
    /// inside the rule (the command goes into an AppleScript string).
    func testInstallChecksBeforeInstalling() {
        let c = LidSleepRule.command(install: true, user: "sam")
        let check = c.range(of: "visudo -cf")!, put = c.range(of: "install -m 0440 -o root -g wheel")!
        XCTAssertLessThan(check.lowerBound, put.lowerBound)
        XCTAssertFalse(LidSleepRule.rule(user: "sam").contains("\""))
        XCTAssertTrue(LidSleepRule.command(install: false, user: "sam").contains("disablesleep 0"))
    }
}
