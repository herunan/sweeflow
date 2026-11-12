import CoreGraphics

/// Where windows go on the virtual display (pure geometry, so it can be tested).
enum Placement {
    static func overlap(_ a: CGRect, _ b: CGRect) -> CGFloat {
        let i = a.intersection(b)
        return i.isNull ? 0 : i.width * i.height
    }

    /// `frame`'s origin, or — when it's mostly (85 %+) under one of `others` — the spot in `area`
    /// where it overlaps them least (on a 120×90 pt grid, top-left first). Only spots where no
    /// single other window covers 85 %+ of it, and where it covers 85 %+ of no other window,
    /// count: with many windows no spot is free, and a window moved over another made that one
    /// move in turn, round and round. With no such spot it stays where it is.
    static func uncovered(_ frame: CGRect, others: [CGRect], in area: CGRect) -> CGPoint {
        let size = frame.width * frame.height
        guard others.contains(where: { overlap(frame, $0) > size * 0.85 }),
              frame.width < area.width, frame.height < area.height else { return frame.origin }
        var best: CGPoint?, bestOverlap = CGFloat.infinity
        for y in stride(from: area.minY, through: area.maxY - frame.height, by: 90) {
            for x in stride(from: area.minX, through: area.maxX - frame.width, by: 120) {
                let r = CGRect(x: x, y: y, width: frame.width, height: frame.height)
                let buries = others.contains { o in
                    let i = overlap(r, o)
                    return i > 0 && (i >= size * 0.85 || i >= o.width * o.height * 0.85)
                }
                if buries { continue }
                let o = others.reduce(0) { $0 + overlap(r, $1) }
                if o < bestOverlap { best = r.origin; bestOverlap = o }
                if o == 0 { break }
            }
            if bestOverlap == 0 { break }
        }
        return best ?? frame.origin
    }
}
