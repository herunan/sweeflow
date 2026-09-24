import numpy as np
from PIL import Image

B = 40  # sprite box, px at 2x
def load(p): return np.asarray(Image.open(p).convert("RGB")).astype(np.float64)

def fit(img, cx, cy):
    """Locate circle centre/radius around (cx, cy) by difference from the local background."""
    box = img[cy-24:cy+24, cx-24:cx+24]
    bg = np.median(np.concatenate([box[:4].reshape(-1,3), box[-4:].reshape(-1,3)]), axis=0)
    d = np.abs(box - bg).sum(axis=2) > 24
    ys, xs = np.nonzero(d)
    ccx, ccy = xs.mean() + cx - 24 + 0.5, ys.mean() + cy - 24 + 0.5
    r = np.sqrt(d.sum() / np.pi)
    return ccx, ccy, r, bg

def extract(img, cx, cy):
    ccx, ccy, r, bg = fit(img, cx, cy)
    x0, y0 = int(round(ccx)) - B//2, int(round(ccy)) - B//2
    px = img[y0:y0+B, x0:x0+B]
    # Analytic anti-aliased circle coverage (4x4 supersampling) as alpha.
    ss = 4
    o = (np.arange(ss) + 0.5) / ss
    yy, xx = np.mgrid[0:B, 0:B].astype(np.float64)
    a = np.zeros((B, B))
    for sy in o:
        for sx in o:
            a += ((xx + x0 + sx - ccx)**2 + (yy + y0 + sy - ccy)**2 <= (r - 0.25)**2)
    a /= ss * ss
    # Un-mix from the background: P = a*F + (1-a)*bg  →  premultiplied aF = P - (1-a)*bg
    pm = np.clip(px - (1 - a[..., None]) * bg, 0, 255)
    pm = np.minimum(pm, a[..., None] * 255)
    return np.dstack([pm, a * 255]), r

def unpremul(s):
    a = s[..., 3:4] / 255
    return np.where(a > 0, s[..., :3] / np.maximum(a, 1e-6), 0)

inactive = load("m1.png");  act = load("g-active.png"); hov = load("g-hover.png"); prs = load("g-press-301.png")
ic = [(660, 376), (706, 376), (752, 376)]     # window at (304,162)pt
ac = [(602, 318), (648, 318), (694, 318)]     # window at (275,133)pt
rows = []
rows.append([extract(inactive, x, y)[0] for x, y in ic])
rows.append([extract(act, x, y)[0] for x, y in ac])
hover = [extract(hov, x, y) for x, y in ac]
rows.append([h[0] for h in hover])
pclose, pr = extract(prs, *ac[0])
# Derive pressed min/zoom from how close changed: lighten toward white by the same amount, grow by the same ratio.
hc, hr = hover[0]
m = (hc[..., 3] > 250) & (pclose[..., 3] > 250)
uh, up = unpremul(hc)[m], unpremul(pclose)[m]
ok = uh < 225   # ignore saturated channels (close is already near-max red)
t = float(np.clip(np.median(((up - uh) / np.maximum(255 - uh, 1))[ok]), 0, 1))
grow = pr / hr
print("pressed lighten", t, "grow", grow, "radius", hr, pr)
pressed = [pclose]
for s, _ in hover[1:]:
    rgb = unpremul(s); rgb = rgb + (255 - rgb) * t
    im = Image.fromarray(np.dstack([rgb, s[..., 3]]).clip(0, 255).astype(np.uint8), "RGBA")
    n = int(round(B * grow)); im = im.resize((n, n), Image.LANCZOS)
    off = (n - B) // 2; im = im.crop((off, off, off + B, off + B))
    q = np.asarray(im).astype(np.float64)
    pressed.append(np.dstack([q[..., :3] * q[..., 3:4] / 255, q[..., 3]]))
rows.append(pressed)
sheet = np.concatenate([np.concatenate(r, axis=1) for r in rows], axis=0)
Image.fromarray(sheet.clip(0, 255).astype(np.uint8), "RGBA").save("traffic-lights.png")   # premultiplied RGBA
# Preview (un-premultiplied over a dark title bar and a light one).
pv = unpremul(sheet); a = sheet[..., 3:4] / 255
for name, bgc in [("dark", 45), ("light", 235)]:
    Image.fromarray((pv * a + bgc * (1 - a)).clip(0, 255).astype(np.uint8)).resize((120*4, 160*4), Image.NEAREST).save(f"tl-preview-{name}.png")
print("sheet", sheet.shape)
