//! Draws macOS (Golden Gate) window buttons: close / minimise / zoom, with the same states as
//! on the Mac: coloured when the window is active, grey when not, glyphs while the pointer is
//! over the group, lighter while pressed. Colours and geometry were sampled from AppKit's own
//! rendering of the buttons.

/// Circle radius and glyph geometry, in points.
const RADIUS: f64 = 6.0;
const STROKE: f64 = 0.62; // half stroke width
const CROSS: f64 = 2.6; // × arm half-extent (before round caps)
const BAR: f64 = 3.4; // − and + half-length (before round caps)

const FILL: [[u8; 3]; 3] = [[237, 106, 94], [250, 228, 65], [89, 239, 80]];
const FILL_PRESSED: [[u8; 3]; 3] = [[240, 147, 137], [251, 235, 116], [134, 243, 127]];
const GLYPH: [[u8; 3]; 3] = [[140, 26, 16], [151, 128, 36], [59, 140, 35]];
const GLYPH_PRESSED: [[u8; 3]; 3] = [[105, 18, 10], [112, 95, 27], [44, 104, 26]];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct State {
    pub active: bool,
    /// Pointer is over the button group (shows glyphs).
    pub hover: bool,
    /// Button held down with the pointer still on it.
    pub pressed: Option<usize>,
    pub dark_titlebar: bool,
}

/// Union of the three button rects (x, y, w, h in window points), or None if any is missing.
pub fn group_rect(buttons: &[[f32; 4]; 3]) -> Option<[f64; 4]> {
    if buttons.iter().any(|r| r[2] <= 0.0 || r[3] <= 0.0) {
        return None;
    }
    let x0 = buttons.iter().map(|r| r[0] as f64).fold(f64::MAX, f64::min);
    let y0 = buttons.iter().map(|r| r[1] as f64).fold(f64::MAX, f64::min);
    let x1 = buttons.iter().map(|r| (r[0] + r[2]) as f64).fold(f64::MIN, f64::max);
    let y1 = buttons.iter().map(|r| (r[1] + r[3]) as f64).fold(f64::MIN, f64::max);
    Some([x0.floor(), y0.floor(), (x1 - x0.floor()).ceil(), (y1 - y0.floor()).ceil()])
}

fn seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

fn in_glyph(i: usize, p: (f64, f64)) -> bool {
    let d = match i {
        0 => seg_dist(p, (-CROSS, -CROSS), (CROSS, CROSS)).min(seg_dist(p, (-CROSS, CROSS), (CROSS, -CROSS))),
        1 => seg_dist(p, (-BAR, 0.0), (BAR, 0.0)),
        _ => seg_dist(p, (-BAR, 0.0), (BAR, 0.0)).min(seg_dist(p, (0.0, -BAR), (0.0, BAR))),
    };
    d <= STROKE
}

/// Renders the group into premultiplied ARGB8888 (wl_shm byte order: B, G, R, A).
/// `rect` is the group rect from [`group_rect`]; returns (pixel width, pixel height, pixels).
pub fn render(buttons: &[[f32; 4]; 3], rect: [f64; 4], scale: f64, st: State) -> (u32, u32, Vec<u8>) {
    let pw = (rect[2] * scale).round().max(1.0) as u32;
    let ph = (rect[3] * scale).round().max(1.0) as u32;
    let mut out = vec![0u8; (pw * ph * 4) as usize];
    const SS: usize = 4; // 4x4 supersampling for anti-aliasing
    let inactive: [u8; 4] = if st.dark_titlebar { [255, 255, 255, 36] } else { [0, 0, 0, 30] };

    for (i, b) in buttons.iter().enumerate() {
        let cx = (b[0] as f64 + b[2] as f64 / 2.0 - rect[0]) * scale;
        let cy = (b[1] as f64 + b[3] as f64 / 2.0 - rect[1]) * scale;
        let r_px = RADIUS * scale;
        let pressed = st.pressed == Some(i);
        let coloured = st.active || st.hover || pressed;
        let fill = if pressed { FILL_PRESSED[i] } else { FILL[i] };
        let glyph = if pressed { GLYPH_PRESSED[i] } else { GLYPH[i] };
        let show_glyph = st.hover || pressed;

        let x0 = (cx - r_px - 1.0).floor().max(0.0) as u32;
        let x1 = ((cx + r_px + 1.0).ceil() as u32).min(pw);
        let y0 = (cy - r_px - 1.0).floor().max(0.0) as u32;
        let y1 = ((cy + r_px + 1.0).ceil() as u32).min(ph);
        for y in y0..y1 {
            for x in x0..x1 {
                let (mut r, mut g, mut bl, mut a) = (0f64, 0f64, 0f64, 0f64);
                for sy in 0..SS {
                    for sx in 0..SS {
                        // Sample position relative to the circle centre, in points.
                        let px = (x as f64 + (sx as f64 + 0.5) / SS as f64 - cx) / scale;
                        let py = (y as f64 + (sy as f64 + 0.5) / SS as f64 - cy) / scale;
                        if px * px + py * py > RADIUS * RADIUS {
                            continue;
                        }
                        let (c, alpha) = if !coloured {
                            ([inactive[0], inactive[1], inactive[2]], inactive[3] as f64 / 255.0)
                        } else if show_glyph && in_glyph(i, (px, py)) {
                            (glyph, 1.0)
                        } else {
                            (fill, 1.0)
                        };
                        r += c[0] as f64 * alpha;
                        g += c[1] as f64 * alpha;
                        bl += c[2] as f64 * alpha;
                        a += alpha;
                    }
                }
                let n = (SS * SS) as f64;
                let o = ((y * pw + x) * 4) as usize;
                out[o] = (bl / n).round() as u8;
                out[o + 1] = (g / n).round() as u8;
                out[o + 2] = (r / n).round() as u8;
                out[o + 3] = (a / n * 255.0).round() as u8;
            }
        }
    }
    (pw, ph, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders every state; set SWEEFLOW_LIGHTS_PNG=path to also dump a contact sheet.
    #[test]
    fn renders_all_states() {
        let buttons = [[8.0, 8.0, 16.0, 16.0], [31.0, 8.0, 16.0, 16.0], [54.0, 8.0, 16.0, 16.0]];
        let rect = group_rect(&buttons).unwrap();
        let states = [
            State { active: true, hover: false, pressed: None, dark_titlebar: true },
            State { active: true, hover: true, pressed: None, dark_titlebar: true },
            State { active: true, hover: true, pressed: Some(0), dark_titlebar: true },
            State { active: false, hover: false, pressed: None, dark_titlebar: true },
            State { active: false, hover: false, pressed: None, dark_titlebar: false },
        ];
        let mut sheet = Vec::new();
        let mut size = (0, 0);
        for st in states {
            let (w, h, px) = render(&buttons, rect, 4.0, st);
            assert_eq!(px.len(), (w * h * 4) as usize);
            assert!(px.chunks(4).any(|p| p[3] > 0), "state {st:?} drew nothing");
            size = (w, size.1 + h);
            sheet.extend(px);
        }
        if let Some(path) = std::env::var_os("SWEEFLOW_LIGHTS_PNG") {
            // BGRA premultiplied → RGBA over a mid-grey title bar for viewing.
            let rgba: Vec<u8> = sheet
                .chunks(4)
                .flat_map(|p| {
                    let a = p[3] as u32;
                    let over = |c: u8| (c as u32 + 0x30 * (255 - a) / 255).min(255) as u8;
                    [over(p[2]), over(p[1]), over(p[0]), 255]
                })
                .collect();
            let f = std::fs::File::create(path).unwrap();
            let mut enc = png::Encoder::new(f, size.0, size.1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
    }
}
