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

    func testAWindowUnderABiggerOneMovesToFreeRoom() {
        let w = CGRect(x: 0, y: 30, width: 960, height: 600)
        let big = CGRect(x: 0, y: 30, width: 1200, height: 700)
        let p = Placement.uncovered(w, others: [big], in: area)
        let moved = CGRect(origin: p, size: w.size)
        XCTAssertNotEqual(p, w.origin)
        XCTAssertEqual(Placement.overlap(moved, big), 0)
        XCTAssertTrue(area.contains(moved))
    }

    func testAWindowStaysWhenTheAreaIsFullOfWindowsItsSize() {
        // 20 windows of the same size, one on every spot of the grid: any spot buries one.
        let room = CGRect(x: 0, y: 0, width: 1440, height: 870)
        var others: [CGRect] = []
        for y in stride(from: 0.0, through: 270.0, by: 90) {
            for x in stride(from: 0.0, through: 480.0, by: 120) {
                others.append(CGRect(x: x, y: y, width: 960, height: 600))
            }
        }
        XCTAssertEqual(others.count, 20)
        let w = CGRect(x: 0, y: 0, width: 960, height: 600)
        XCTAssertEqual(Placement.uncovered(w, others: others, in: room), w.origin)
    }

    func testNeverMovesOntoASmallerWindow() {
        // The spots overlapping least lie over the small window: those don't count.
        let room = CGRect(x: 0, y: 0, width: 2000, height: 700)
        let big = CGRect(x: 0, y: 0, width: 1000, height: 700)
        let little = CGRect(x: 1200, y: 100, width: 400, height: 300)
        let w = CGRect(x: 0, y: 0, width: 960, height: 600)
        let p = Placement.uncovered(w, others: [big, little], in: room)
        let moved = CGRect(origin: p, size: w.size)
        XCTAssertNotEqual(p, w.origin)
        XCTAssertLessThan(Placement.overlap(moved, little), little.width * little.height * 0.85)
        XCTAssertLessThan(Placement.overlap(moved, big), w.width * w.height * 0.85)
        XCTAssertTrue(room.contains(moved))
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
