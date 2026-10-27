# Usage: python3 assets/tools/gen-logo.py assets/sweeflow.svg   (then mac/make-icons.sh for the Mac's PNGs)
# Generates assets/sweeflow.svg: an apple wrapped like a sweet, as one silhouette filling the
# icon (no background), in the theme's text colour: white on a dark theme (KDE recolours icons
# that use the ColorScheme-Text class, as Breeze's own; elsewhere it's the white given here).
import sys

TILE = 'M34 8H94A26 26 0 0 1 120 34V94A26 26 0 0 1 94 120H34A26 26 0 0 1 8 94V34A26 26 0 0 1 34 8Z'
APPLE = 'M0 -12 C6 -18 22 -19.5 24 -4 C25.5 8 17.5 22 9 22 C5.5 22 3 20.5 0 20.5 C-3 20.5 -5.5 22 -9 22 C-17.5 22 -25.5 8 -24 -4 C-22 -19.5 -6 -18 0 -12 Z'
STEM = 'M-1.7 -12 L-1.7 -17 C-1.7 -20 -.5 -22.5 1.5 -24.5 L3.7 -22.6 C2.3 -21 1.7 -19.5 1.7 -17 L1.7 -12 Z'
LEAF = 'M5 -19 C8 -27 14 -30 21 -28.5 C18 -21 12 -18 5 -19 Z'

def wrapper_end(side, pinch=23, reach=20, half=17, lobes=3, bulge=7):
    """A twisted wrapper end: a fan opening from the pinch, its outer edge in soft lobes."""
    s = side
    x0, x1 = s * pinch, s * (pinch + reach)
    d = f'M{x0} -3 C{x0 + s*6} -4.5 {x1 - s*7} {-half - 1} {x1} {-half} '
    for i in range(lobes):
        y0 = -half + 2 * half * i / lobes
        y1 = -half + 2 * half * (i + 1) / lobes
        d += f'C{x1 + s*bulge} {y0 + 1:.2f} {x1 + s*bulge} {y1 - 1:.2f} {x1} {y1:.2f} '
    return d + f'C{x1 - s*7} {half + 1} {x0 + s*6} 4.5 {x0} 3 Z'

shapes = [wrapper_end(-1), wrapper_end(1), APPLE, STEM, LEAF]
def logo(fill_attrs, defs='', backdrop='', scale=1.2):
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">\n' + defs + backdrop
            + f'  <g transform="translate(64 64) rotate(-20) scale({scale})" {fill_attrs}>\n'
            + ''.join(f'    <path d="{d}"/>\n' for d in shapes) + '  </g>\n</svg>\n')

TILE = 'M34 8H94A26 26 0 0 1 120 34V94A26 26 0 0 1 94 120H34A26 26 0 0 1 8 94V34A26 26 0 0 1 34 8Z'
out = sys.argv[1]
open(out, 'w').write(logo('class="ColorScheme-Text" fill="currentColor"',
                          '  <style id="current-color-scheme" type="text/css">.ColorScheme-Text { color: #fcfcfc; }</style>\n'))
# The Mac's: an app icon (white on a violet rounded square, as Mac app icons are) and the
# menu-bar icon (black on clear: macOS draws it as a template, in the menu bar's colour).
base = out.rsplit('.', 1)[0]
open(base + '-mac-app.svg', 'w').write(logo('fill="#fff"',
    '  <defs><linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#a855f7"/><stop offset="1" stop-color="#6d28d9"/></linearGradient></defs>\n',
    f'  <path d="{TILE}" fill="url(#bg)"/>\n', scale=0.98))
open(base + '-mac-menu.svg', 'w').write(logo('fill="#000"'))
