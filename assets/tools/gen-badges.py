#!/usr/bin/env python3
"""Makes the README's "Tested on" badges (assets/badges/*.svg): distro with its logo, then
desktop, then Wayland or X11. Three parts, which shields.io can't do. Logos are Simple Icons
(CC0), fetched from cdn.simpleicons.org. Text widths are measured with DejaVu Sans, the closest
free face to Verdana, which the badges ask for first.

    python3 assets/tools/gen-badges.py
"""
import re
import urllib.request
from pathlib import Path

from PIL import ImageFont

OUT = Path(__file__).resolve().parent.parent / "badges"
FONT = ImageFont.truetype("/usr/share/fonts/TTF/DejaVuSans.ttf", 11)

DISTRO, DESKTOP = "#2D333B", "#444C56"
SESSION = {"Wayland": "#8B5CF6", "X11": "#2F81F7"}

BADGES = [
    ("cachyos-plasma", "CachyOS", "cachyos", "KDE Plasma", "Wayland"),
    ("cachyos-gnome", "CachyOS", "cachyos", "GNOME", "Wayland"),
    ("ubuntu-gnome", "Ubuntu", "ubuntu", "GNOME", "Wayland"),
    ("mint-cinnamon", "Linux Mint", "linuxmint", "Cinnamon", "X11"),
    ("omarchy-hyprland", "Omarchy", "omarchy", "Hyprland", "Wayland"),
]

PAD, LOGO, GAP, H = 6, 14, 4, 20


def logo_path(slug: str) -> str:
    req = urllib.request.Request(f"https://cdn.simpleicons.org/{slug}", headers={"User-Agent": "gen-badges"})
    svg = urllib.request.urlopen(req, timeout=20).read().decode()
    return re.search(r'<path d="([^"]+)"', svg).group(1)


def width(text: str) -> int:
    return round(FONT.getlength(text))


def badge(distro: str, slug: str, desktop: str, session: str) -> str:
    w1 = PAD + LOGO + GAP + width(distro) + PAD
    w2 = PAD + width(desktop) + PAD
    w3 = PAD + width(session) + PAD
    total = w1 + w2 + w3
    label = f"{distro} {desktop}, {session}"

    def text(x: float, s: str) -> str:
        return (f'<text x="{x}" y="15" fill="#010101" fill-opacity=".3">{s}</text>'
                f'<text x="{x}" y="14">{s}</text>')

    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{total}" height="{H}" role="img" aria-label="{label}">
<title>{label}</title>
<linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
<clipPath id="r"><rect width="{total}" height="{H}" rx="3" fill="#fff"/></clipPath>
<g clip-path="url(#r)">
<rect width="{w1}" height="{H}" fill="{DISTRO}"/>
<rect x="{w1}" width="{w2}" height="{H}" fill="{DESKTOP}"/>
<rect x="{w1 + w2}" width="{w3}" height="{H}" fill="{SESSION[session]}"/>
<rect width="{total}" height="{H}" fill="url(#s)"/>
</g>
<path transform="translate({PAD} 3) scale({LOGO / 24})" fill="#fff" d="{logo_path(slug)}"/>
<g fill="#fff" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
{text(PAD + LOGO + GAP, distro)}
{text(w1 + PAD, desktop)}
{text(w1 + w2 + PAD, session)}
</g>
</svg>
'''


OUT.mkdir(exist_ok=True)
for name, distro, slug, desktop, session in BADGES:
    (OUT / f"{name}.svg").write_text(badge(distro, slug, desktop, session))
    print(OUT / f"{name}.svg")
