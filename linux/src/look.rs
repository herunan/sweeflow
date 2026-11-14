//! How Sweeflow's own windows (Link a Mac, a Mac's prompt) should look on this desktop: its
//! colours, fonts and corners, read from GNOME, Omarchy or Plasma, so they sit there like the
//! desktop's own dialogs. look.slint holds the Slint side: the `Look` global these values go
//! into, and the controls drawn with it.

use std::{path::PathBuf, process::Command};

pub type Rgb = (u8, u8, u8);

pub struct Look {
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
    /// Text on the accent colour.
    pub on_accent: Rgb,
    /// Corner radius of buttons and boxes, in pixels.
    pub radius: f32,
    /// Font families; empty: the toolkit's own.
    pub font: String,
    pub mono: String,
    /// Controls drawn as outlines instead of filled shapes.
    pub outlined: bool,
}

impl Default for Look {
    /// Where the desktop says nothing: dark, with Sweeflow's violet.
    fn default() -> Self {
        Look {
            bg: (0x1e, 0x1e, 0x22),
            fg: (0xf2, 0xf2, 0xf4),
            accent: (0x8b, 0x5c, 0xf6),
            on_accent: (0xff, 0xff, 0xff),
            radius: 8.0,
            font: String::new(),
            mono: family("monospace"),
            outlined: false,
        }
    }
}

/// This desktop's look.
pub fn detect() -> Look {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let hyprland = desktop.contains("Hyprland") || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some();
    if hyprland {
        if let Some(look) = omarchy() {
            return look;
        }
    }
    if desktop.contains("GNOME") {
        return gnome();
    }
    if desktop.contains("KDE") {
        return plasma().unwrap_or_default();
    }
    Look::default()
}

/// Omarchy: the current theme's colours, its monospace font everywhere, Hyprland's corners
/// (square unless changed), and outlined controls as in its own menus.
fn omarchy() -> Option<Look> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let colors = [".local/state/omarchy/current/theme/colors.toml", ".config/omarchy/current/theme/colors.toml"]
        .iter()
        .find_map(|p| std::fs::read_to_string(home.join(p)).ok())?;
    let get = |key: &str| {
        colors.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| hex(v.trim().trim_matches('"'))).flatten()
        })
    };
    let bg = get("background")?;
    let radius = output("hyprctl", &["-j", "getoption", "decoration:rounding"])
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
        .and_then(|j| j["int"].as_f64())
        .unwrap_or(0.0);
    let mono = family("monospace");
    Some(Look {
        bg,
        fg: get("foreground")?,
        accent: get("accent")?,
        on_accent: bg,
        radius: radius as f32,
        font: mono.clone(),
        mono,
        outlined: true,
    })
}

/// GNOME: Adwaita's window colours for the light or dark style in use, the accent colour chosen
/// in Settings, and the interface font.
fn gnome() -> Look {
    let setting = |key: &str| output("gsettings", &["get", "org.gnome.desktop.interface", key]).map(|v| v.trim_matches('\'').to_string());
    let dark = setting("color-scheme").is_some_and(|s| s == "prefer-dark");
    let accent = match setting("accent-color").as_deref() {
        Some("teal") => (0x21, 0x90, 0xa4),
        Some("green") => (0x3a, 0x94, 0x4a),
        Some("yellow") => (0xc8, 0x88, 0x00),
        Some("orange") => (0xed, 0x5b, 0x00),
        Some("red") => (0xe6, 0x2d, 0x42),
        Some("pink") => (0xd5, 0x61, 0x99),
        Some("purple") => (0x91, 0x41, 0xac),
        Some("slate") => (0x6f, 0x83, 0x96),
        _ => (0x35, 0x84, 0xe4),
    };
    // "Cantarell 11": the family, without the size. Not GNOME's default, Adwaita Sans: Slint draws
    // that one squeezed narrow (it is a variable font), so the toolkit's own sans stands in.
    let font = setting("font-name")
        .map(|f| f.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ').to_string())
        .filter(|f| f != "Adwaita Sans")
        .unwrap_or_default();
    Look {
        bg: if dark { (0x22, 0x22, 0x26) } else { (0xfa, 0xfa, 0xfb) },
        fg: if dark { (0xff, 0xff, 0xff) } else { (0x2e, 0x2e, 0x33) },
        accent,
        on_accent: (0xff, 0xff, 0xff),
        radius: 9.0,
        font,
        mono: family("monospace"),
        outlined: false,
    }
}

/// Plasma: the colour scheme's window, text and highlight colours, and the general font.
fn plasma() -> Option<Look> {
    let read = |group: &str, key: &str| output("kreadconfig6", &["--file", "kdeglobals", "--group", group, "--key", key]);
    let rgb = |group: &str, key: &str| {
        let v = read(group, key)?;
        let mut n = v.split(',').map(|n| n.trim().parse::<u8>());
        Some((n.next()?.ok()?, n.next()?.ok()?, n.next()?.ok()?))
    };
    Some(Look {
        bg: rgb("Colors:Window", "BackgroundNormal")?,
        fg: rgb("Colors:Window", "ForegroundNormal")?,
        accent: rgb("Colors:Selection", "BackgroundNormal")?,
        on_accent: rgb("Colors:Selection", "ForegroundNormal")?,
        radius: 5.0,
        font: read("General", "font").and_then(|f| f.split(',').next().map(str::to_string)).unwrap_or_default(),
        mono: family("monospace"),
        outlined: false,
    })
}

/// "#rrggbb".
fn hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#')?;
    let n = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
    Some((n(0)?, n(2)?, n(4)?))
}

/// What a command prints, when it works and prints something.
fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The font family a fontconfig name ("monospace") stands for here.
fn family(alias: &str) -> String {
    output("fc-match", &["-f", "%{family[0]}", alias]).unwrap_or_default()
}

/// Sweeflow's violet: the accent of its windows everywhere (the Plasma widget's too).
pub const VIOLET: Rgb = (0x8b, 0x5c, 0xf6);

/// Gives a window (whose Slint code exports look.slint's `Look`) this desktop's look.
macro_rules! apply {
    ($ui:expr) => {{
        let look = $crate::look::detect();
        let color = |(r, g, b): (u8, u8, u8)| slint::Color::from_rgb_u8(r, g, b);
        let global = $ui.global::<Look>();
        global.set_bg(color(look.bg));
        global.set_fg(color(look.fg));
        // Sweeflow's own violet on every desktop (the owner's call), not the desktop's accent:
        // the window's colours and font still follow the desktop.
        let _ = (look.accent, look.on_accent);
        global.set_accent(color($crate::look::VIOLET));
        global.set_on_accent(color((0xff, 0xff, 0xff)));
        global.set_radius(look.radius);
        global.set_font(look.font.into());
        global.set_mono(look.mono.into());
        global.set_outlined(look.outlined);
    }};
}
pub(crate) use apply;

#[cfg(test)]
mod tests {
    #[test]
    fn hex_colours() {
        assert_eq!(super::hex("#7aa2f7"), Some((0x7a, 0xa2, 0xf7)));
        assert_eq!(super::hex("7aa2f7"), None);
        assert_eq!(super::hex("#7aa2"), None);
    }
}
