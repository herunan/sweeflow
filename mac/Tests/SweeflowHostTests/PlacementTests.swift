import CoreGraphics
import XCTest
@testable import SweeflowHost

final class PlacementTests: XCTestCase {
    let area = CGRect(x: 0, y: 30, width: 3440, height: 1410)

    func testLeavesAWindowThatIsNotCoveredAlone() {
        let w = CGRect(x: 1000, y: 30, width: 960, height: 600)
        let others = [CGRect(x: 0, y: 30, width: 960, height: 600)]
        XCTAssertEqual(Placement.uncovered(w, others: others, in: area), w.origin)
    }

    func testMovesAWindowStackedExactlyOnAnother() {
        let w = CGRect(x: 0, y: 30, width: 960, height: 600)
        let p = Placement.uncovered(w, others: [w], in: area)
        let moved = CGRect(origin: p, size: w.size)
        XCTAssertEqual(Placement.overlap(moved, w), 0)
        XCTAssertTrue(area.contains(moved))
    }

    func testSeveralStackedWindowsEndUpApart() {
        var placed: [CGRect] = []
        for _ in 0..<6 {
            let w = CGRect(x: 0, y: 30, width: 960, height: 600)
            let p = Placement.uncovered(w, others: placed, in: area)
            placed.append(CGRect(origin: p, size: w.size))
        }
        for (i, a) in placed.enumerated() {
            XCTAssertTrue(area.contains(a))
            for b in placed[(i + 1)...] { XCTAssertEqual(Placement.overlap(a, b), 0, "\(a) overlaps \(b)") }
        }
    }

    func testAWindowAsBigAsTheDisplayStaysPut() {
        let w = CGRect(x: 0, y: 30, width: 3440, height: 1410)
        XCTAssertEqual(Placement.uncovered(w, others: [w], in: area), w.origin)
    }
}

final class SettingsTests: XCTestCase {
    func testMergeChangesOnlyWhatWasSent() {
        var c = Config()
        Settings.merge(Settings.Wire(clipboard: false), into: &c)
        XCTAssertFalse(c.clipboard)
        XCTAssertTrue(c.enabled)
        XCTAssertTrue(c.dragDrop)
    }

    func testMergeIgnoresAnUnknownQuality() {
        var c = Config()
        Settings.merge(Settings.Wire(quality: "ultra"), into: &c)
        XCTAssertEqual(c.quality, "sharp")
        Settings.merge(Settings.Wire(quality: "light"), into: &c)
        XCTAssertEqual(c.quality, "light")
    }
}
