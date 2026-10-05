import CoreGraphics

/// Where windows go on the virtual display (pure geometry, so it can be tested).
enum Placement {
    static func overlap(_ a: CGRect, _ b: CGRect) -> CGFloat {
        let i = a.intersection(b)
        return i.isNull ? 0 : i.width * i.height
    }

    /// `frame`'s origin, or — when it's mostly (85 %+) under one of `others` — the spot in `area`
    /// where it overlaps them least (on a 120×90 pt grid, top-left first).
    static func uncovered(_ frame: CGRect, others: [CGRect], in area: CGRect) -> CGPoint {
        let size = frame.width * frame.height
        guard others.contains(where: { overlap(frame, $0) > size * 0.85 }),
              frame.width < area.width, frame.height < area.height else { return frame.origin }
        var best = frame.origin, bestOverlap = CGFloat.infinity
        for y in stride(from: area.minY, through: area.maxY - frame.height, by: 90) {
            for x in stride(from: area.minX, through: area.maxX - frame.width, by: 120) {
                let r = CGRect(x: x, y: y, width: frame.width, height: frame.height)
                let o = others.reduce(0) { $0 + overlap(r, $1) }
                if o < bestOverlap { best = r.origin; bestOverlap = o }
                if o == 0 { break }
            }
            if bestOverlap == 0 { break }
        }
        return best
    }
}
