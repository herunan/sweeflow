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

/// Renders the group into premultiplied ARGB8888 (wl_shm byte order: B, G, R, A) at the
/// sprites' native 2x; returns (pixel width, pixel height, pixels).
pub fn render(buttons: &[[f32; 4]; 3], rect: [f64; 4], st: State) -> (u32, u32, Vec<u8>) {
    let s = SPRITE_SCALE;
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
        let (ox, oy) = (cx - BOX as isize / 2, cy - BOX as isize / 2);
        for y in 0..BOX {
            let dy = oy + y as isize;
            if dy < 0 || dy >= ph as isize {
                continue;
            }
            for x in 0..BOX {
                let dx = ox + x as isize;
                if dx < 0 || dx >= pw as isize {
                    continue;
                }
                let si = ((row * BOX + y) * sh.w + i * BOX + x) * 4;
                let [r, g, bl, a] = [sh.rgba[si], sh.rgba[si + 1], sh.rgba[si + 2], sh.rgba[si + 3]];
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
