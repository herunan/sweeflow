//! Draws macOS (Golden Gate) window buttons: close / minimise / zoom, with the same states as
//! on the Mac: coloured when the window is active, grey when not, glyphs while the pointer is
//! over the group, lighter while pressed. Colours and geometry were sampled from AppKit's own
//! rendering of the buttons.

/// Real macOS Golden Gate window buttons, captured from a live window on the Mac and turned into
/// premultiplied RGBA sprites (assets/extract-traffic-lights.py). 40x40 px boxes at 2x:
/// rows = inactive, active, hover (glyphs), pressed; columns = close, minimise, zoom.
const SHEET_PNG: &[u8] = include_bytes!("../assets/traffic-lights.png");
const BOX: usize = 40;
const SPRITE_SCALE: f64 = 2.0;
/// Room around each 16 pt button rect for the sprite (pressed buttons grow a little).
const MARGIN: f64 = 3.0;

struct Sheet {
    w: usize,
    rgba: Vec<u8>,
}

fn sheet() -> &'static Sheet {
    static SHEET: std::sync::OnceLock<Sheet> = std::sync::OnceLock::new();
    SHEET.get_or_init(|| {
        let mut dec = png::Decoder::new(std::io::Cursor::new(SHEET_PNG));
        dec.set_transformations(png::Transformations::EXPAND);
        let mut r = dec.read_info().expect("traffic-light sheet");
        let mut buf = vec![0; r.output_buffer_size().unwrap_or(0)];
        let info = r.next_frame(&mut buf).expect("traffic-light sheet");
        buf.truncate(info.buffer_size());
        Sheet { w: info.width as usize, rgba: buf }
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct State {
    pub active: bool,
    /// Pointer is over the button group (shows glyphs).
    pub hover: bool,
    /// Button held down with the pointer still on it.
    pub pressed: Option<usize>,
    pub dark_titlebar: bool,
}

/// Union of the three button rects plus a margin (x, y, w, h in window points), or None if
/// any button is missing.
pub fn group_rect(buttons: &[[f32; 4]; 3]) -> Option<[f64; 4]> {
    if buttons.iter().any(|r| r[2] <= 0.0 || r[3] <= 0.0) {
        return None;
    }
    let x0 = buttons.iter().map(|r| r[0] as f64).fold(f64::MAX, f64::min) - MARGIN;
    let y0 = buttons.iter().map(|r| r[1] as f64).fold(f64::MAX, f64::min) - MARGIN;
    let x1 = buttons.iter().map(|r| (r[0] + r[2]) as f64).fold(f64::MIN, f64::max) + MARGIN;
    let y1 = buttons.iter().map(|r| (r[1] + r[3]) as f64).fold(f64::MIN, f64::max) + MARGIN;
    Some([x0.floor(), y0.floor(), (x1 - x0.floor()).ceil(), (y1 - y0.floor()).ceil()])
}

/// How big to draw the buttons, against the current macOS ones (1.0). Apps built for an older
/// macOS keep its smaller buttons (Herdr's are, on the Mac itself), and apps that draw their own
/// choose a size: all of them sit closer together than today's 23 points. The spacing says the
/// size: the buttons have always been spaced in proportion.
pub fn size(buttons: &[[f32; 4]; 3]) -> f64 {
    const PITCH: f64 = 23.0;
    let centre = |b: &[f32; 4]| b[0] as f64 + b[2] as f64 / 2.0;
    let pitch = (centre(&buttons[1]) - centre(&buttons[0])).min(centre(&buttons[2]) - centre(&buttons[1]));
    if pitch <= 0.0 { 1.0 } else { (pitch / PITCH).clamp(0.6, 1.0) }
}

/// Renders the group into premultiplied ARGB8888 (wl_shm byte order: B, G, R, A) at the
/// sprites' native 2x; returns (pixel width, pixel height, pixels).
pub fn render(buttons: &[[f32; 4]; 3], rect: [f64; 4], st: State) -> (u32, u32, Vec<u8>) {
    let s = SPRITE_SCALE;
    let k = size(buttons);
    // The sprite's box at this size.
    let boxed = ((BOX as f64 * k).round() as usize).max(2);
    let pw = (rect[2] * s).round().max(1.0) as usize;
    let ph = (rect[3] * s).round().max(1.0) as usize;
    let mut out = vec![0u8; pw * ph * 4];
    let sh = sheet();
    for (i, b) in buttons.iter().enumerate() {
        let row = if st.pressed == Some(i) {
            3
        } else if st.hover || st.pressed.is_some() {
            2
        } else if st.active {
            1
        } else {
            0
        };
        let cx = ((b[0] as f64 + b[2] as f64 / 2.0 - rect[0]) * s).round() as isize;
        let cy = ((b[1] as f64 + b[3] as f64 / 2.0 - rect[1]) * s).round() as isize;
        let (ox, oy) = (cx - boxed as isize / 2, cy - boxed as isize / 2);
        let sprite = |x: usize, y: usize| -> [u8; 4] {
            let si = ((row * BOX + y) * sh.w + i * BOX + x) * 4;
            [sh.rgba[si], sh.rgba[si + 1], sh.rgba[si + 2], sh.rgba[si + 3]]
        };
        for y in 0..boxed {
            let dy = oy + y as isize;
            if dy < 0 || dy >= ph as isize {
                continue;
            }
            for x in 0..boxed {
                let dx = ox + x as isize;
                if dx < 0 || dx >= pw as isize {
                    continue;
                }
                let [r, g, bl, a] = if boxed == BOX {
                    sprite(x, y)
                } else {
                    // A smaller button: each pixel between its four neighbours in the sprite
                    // (premultiplied, so they mix as they are).
                    let (fx, fy) = ((x as f64 + 0.5) / k - 0.5, (y as f64 + 0.5) / k - 0.5);
                    let (x0, y0) = (fx.floor().clamp(0.0, BOX as f64 - 1.0) as usize, fy.floor().clamp(0.0, BOX as f64 - 1.0) as usize);
                    let (x1, y1) = ((x0 + 1).min(BOX - 1), (y0 + 1).min(BOX - 1));
                    let (tx, ty) = ((fx - x0 as f64).clamp(0.0, 1.0), (fy - y0 as f64).clamp(0.0, 1.0));
                    let (p00, p10, p01, p11) = (sprite(x0, y0), sprite(x1, y0), sprite(x0, y1), sprite(x1, y1));
                    std::array::from_fn(|c| {
                        let top = p00[c] as f64 * (1.0 - tx) + p10[c] as f64 * tx;
                        let bottom = p01[c] as f64 * (1.0 - tx) + p11[c] as f64 * tx;
                        (top * (1.0 - ty) + bottom * ty).round() as u8
                    })
                };
                if a == 0 {
                    continue;
                }
                // Premultiplied "over" onto what is already there.
                let o = (dy as usize * pw + dx as usize) * 4;
                let inv = 255 - a as u32;
                out[o] = (bl as u32 + out[o] as u32 * inv / 255) as u8;
                out[o + 1] = (g as u32 + out[o + 1] as u32 * inv / 255) as u8;
                out[o + 2] = (r as u32 + out[o + 2] as u32 * inv / 255) as u8;
                out[o + 3] = (a as u32 + out[o + 3] as u32 * inv / 255) as u8;
            }
        }
    }
    (pw as u32, ph as u32, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders every state; set SWEEFLOW_LIGHTS_PNG=path to also dump a contact sheet.
    #[test]
    fn renders_all_states() {
        let buttons = [[8.0, 8.0, 16.0, 16.0], [31.0, 8.0, 16.0, 16.0], [54.0, 8.0, 16.0, 16.0]];
        let rect = group_rect(&buttons).unwrap();
        assert_eq!(size(&buttons), 1.0, "today's spacing: full size");
        // Herdr's (an older toolkit's buttons, 20 points apart): smaller, as on the Mac.
        let older = [[13.0, 17.0, 12.0, 14.0], [33.0, 17.0, 12.0, 14.0], [53.0, 17.0, 12.0, 14.0]];
        assert!((size(&older) - 20.0 / 23.0).abs() < 1e-6);
        // Spaced tightly (a browser's compact toolbar): smaller still, and a clear gap stays,
        // nothing drawn half way between the first two.
        let tight = [[6.0, 9.0, 16.0, 16.0], [23.5, 9.0, 16.0, 16.0], [41.0, 9.0, 16.0, 16.0]];
        assert!((size(&tight) - 17.5 / 23.0).abs() < 1e-6);
        let tight_rect = group_rect(&tight).unwrap();
        let (w, h, px) = render(&tight, tight_rect, State { active: true, hover: false, pressed: None, dark_titlebar: true });
        assert_eq!(px.len(), (w * h * 4) as usize);
        let gap_x = ((22.75 - tight_rect[0]) * SPRITE_SCALE) as usize;
        assert!((0..h as usize).all(|y| px[(y * w as usize + gap_x) * 4 + 3] == 0), "tightly spaced buttons touch");
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
            let (w, h, px) = render(&buttons, rect, st);
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
