# Usage: python3 assets/tools/gen-logo.py assets/sweeflow.svg
# Generates assets/sweeflow.svg: tapered swooshes are polygons offset from a cubic centreline.
import math, sys

def bez(p0, p1, p2, p3, t):
    u = 1 - t
    return tuple(u**3*a + 3*u*u*t*b + 3*u*t*t*c + t**3*d for a, b, c, d in zip(p0, p1, p2, p3))

def swoosh(p0, p1, p2, p3, w0, w1, n=40):
    """Centreline from p0 (thick, w0) to p3 (thin, w1); returns a closed path."""
    pts = [bez(p0, p1, p2, p3, i / n) for i in range(n + 1)]
    left, right = [], []
    for i, (x, y) in enumerate(pts):
        a = pts[max(i - 1, 0)]; b = pts[min(i + 1, n)]
        dx, dy = b[0] - a[0], b[1] - a[1]
        l = math.hypot(dx, dy) or 1
        nx, ny = -dy / l, dx / l
        t = i / n
        w = (w0 + (w1 - w0) * t ** 0.8) / 2
        left.append((x + nx * w, y + ny * w)); right.append((x - nx * w, y - ny * w))
    # round head at p0
    d = "M%.2f %.2f " % left[0]
    d += " ".join("L%.2f %.2f" % p for p in left[1:])
    d += " " + " ".join("L%.2f %.2f" % p for p in reversed(right))
    d += " A%.2f %.2f 0 0 1 %.2f %.2f Z" % (w0 / 2, w0 / 2, *left[0])
    return d

waves = [
    # (start under the apple, controls, tail), thick, thin, colour
    (((66, 58), (50, 50), (36, 66), (8, 54)), 11, 1.2, "#ffa040"),
    (((64, 77), (46, 69), (32, 87), (2, 75)), 12, 1.2, "#ff5a52"),
    (((68, 96), (54, 90), (42, 104), (18, 96)), 10, 1.2, "#e8306a"),
]
leaf_wave = (((90, 24), (82, 20), (76, 28), (62, 22)), 4.5, 0.6, "#3fcf74")

out = ['<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">', '  <defs>',
       '    <linearGradient id="skin" x1="0.15" y1="0" x2="0.85" y2="1"><stop offset="0" stop-color="#ffa13d"/><stop offset="0.45" stop-color="#ff4b58"/><stop offset="1" stop-color="#b8155a"/></linearGradient>',
       '    <radialGradient id="glow" cx="0.34" cy="0.3" r="0.55"><stop offset="0" stop-color="#fff" stop-opacity="0.32"/><stop offset="1" stop-color="#fff" stop-opacity="0"/></radialGradient>',
       '    <linearGradient id="leaf" x1="0" y1="1" x2="1" y2="0"><stop offset="0" stop-color="#14a04a"/><stop offset="1" stop-color="#5fe08a"/></linearGradient>']
for i, (pts, w0, w1, c) in enumerate(waves + [leaf_wave]):
    x0, x1 = pts[0][0], pts[3][0]
    out.append(f'    <linearGradient id="w{i}" gradientUnits="userSpaceOnUse" x1="{x0 - 8}" y1="0" x2="{x1}" y2="0"><stop offset="0" stop-color="{c}"/><stop offset="1" stop-color="{c}" stop-opacity="0.05"/></linearGradient>')
out.append('  </defs>')
out.append('  <!-- speed waves streaming out from behind the apple and its leaf -->')
for i, (pts, w0, w1, c) in enumerate(waves + [leaf_wave]):
    out.append(f'  <path fill="url(#w{i})" d="{swoosh(*pts, w0, w1)}"/>')
apple = "M84 42 C77 34 60 33 52 47 C42 64 48 97 63 110 C70 116 77 115 84 111 C91 115 98 116 105 110 C120 97 126 64 116 47 C108 33 91 34 84 42 Z"
out.append(f'  <path fill="url(#skin)" d="{apple}"/>')
out.append(f'  <path fill="url(#glow)" d="{apple}"/>')
out.append('  <path fill="url(#leaf)" d="M88 33 C88 21 97 12 110 11 C110 24 101 32 88 33 Z"/>')
out.append('</svg>')
open(sys.argv[1], 'w').write("\n".join(out) + "\n")
