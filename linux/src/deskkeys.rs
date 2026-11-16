//! Which keys stay the desktop's while a Mac window has the keyboard and the pointer is over a
//! Mac window (then every other shortcut goes to the Mac, Super as Cmd). The rule, the same on
//! every desktop: shortcuts with a letter are the app's (Cmd+C, Cmd+T, Cmd+F, Opt+letter for
//! accents), and so are arrows with Cmd or Opt (moving through text); the ones for windows and
//! the desktop are the desktop's: switching windows (Super+Tab, Alt+Tab), the launcher
//! (Super+Space), workspaces and the like (digits, Page Up/Down, F-keys… with Super or Alt;
//! Ctrl+Alt+arrows), and the media and brightness keys. Of those, only the ones the desktop
//! actually uses are taken from the Mac where that can be known (Plasma says, Hyprland and X11
//! X11 gets the key back and passes on what it doesn't use; GNOME runs none for an app, so its
//! window keys are let through from the next press). Not on Hyprland, where the desktop's keys
//! stay first (see ui.rs sync_inhibitor).

use std::process::Command;

// Linux input event codes (the keyboard's raw codes on Wayland; X11 keycodes are these + 8).
const KEY_ESC: u32 = 1;
const KEY_1: u32 = 2;
const KEY_0: u32 = 11;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_SPACE: u32 = 57;
const KEY_F1: u32 = 59;
const KEY_F10: u32 = 68;
const KEY_F11: u32 = 87;
const KEY_F12: u32 = 88;
const KEY_SYSRQ: u32 = 99;
const KEY_HOME: u32 = 102;
const KEY_UP: u32 = 103;
const KEY_PAGEUP: u32 = 104;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_END: u32 = 107;
const KEY_DOWN: u32 = 108;
const KEY_PAGEDOWN: u32 = 109;
const KEY_DELETE: u32 = 111;
const KEY_MUTE: u32 = 113;
const KEY_VOLUMEDOWN: u32 = 114;
const KEY_VOLUMEUP: u32 = 115;
const KEY_NEXTSONG: u32 = 163;
const KEY_PLAYPAUSE: u32 = 164;
const KEY_PREVIOUSSONG: u32 = 165;
const KEY_STOPCD: u32 = 166;
const KEY_BRIGHTNESSDOWN: u32 = 224;
const KEY_BRIGHTNESSUP: u32 = 225;

/// The modifiers held with a key.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mods {
    pub logo: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
}

/// Media, volume, brightness and the screenshot key: the desktop's, whatever is held.
pub fn is_media(raw: u32) -> bool {
    matches!(
        raw,
        KEY_MUTE | KEY_VOLUMEDOWN | KEY_VOLUMEUP | KEY_NEXTSONG | KEY_PLAYPAUSE | KEY_PREVIOUSSONG | KEY_STOPCD
            | KEY_BRIGHTNESSDOWN | KEY_BRIGHTNESSUP | KEY_SYSRQ
    )
}

/// Whether a key (raw code) with these modifiers is one for windows or the desktop (see the
/// module's rule), to be left to the desktop if it uses it.
pub fn is_desktop_key(raw: u32, m: Mods) -> bool {
    if is_media(raw) {
        return true;
    }
    let arrow = matches!(raw, KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT);
    if arrow {
        // Cmd/Opt(+Shift)+arrows move and select through text on the Mac; with Ctrl and
        // another modifier they're the desktop's (workspaces, moving windows).
        return m.ctrl && (m.alt || m.logo);
    }
    if !(m.logo || m.alt) {
        return false;
    }
    matches!(raw, KEY_TAB | KEY_SPACE | KEY_ESC | KEY_ENTER | KEY_HOME | KEY_END | KEY_PAGEUP | KEY_PAGEDOWN | KEY_DELETE)
        || (KEY_1..=KEY_0).contains(&raw)
        || (KEY_F1..=KEY_F10).contains(&raw)
        || raw == KEY_F11
        || raw == KEY_F12
}

/// The key as Qt counts it (Qt::META | Qt::Key_Tab…), which is how Plasma's shortcuts are kept.
pub fn qt_key(raw: u32, m: Mods) -> Option<i32> {
    let key = match raw {
        KEY_TAB => 0x0100_0001,
        KEY_SPACE => 0x20,
        KEY_ESC => 0x0100_0000,
        KEY_ENTER => 0x0100_0004,
        KEY_DELETE => 0x0100_0007,
        KEY_SYSRQ => 0x0100_0009,
        KEY_HOME => 0x0100_0010,
        KEY_END => 0x0100_0011,
        KEY_LEFT => 0x0100_0012,
        KEY_UP => 0x0100_0013,
        KEY_RIGHT => 0x0100_0014,
        KEY_DOWN => 0x0100_0015,
        KEY_PAGEUP => 0x0100_0016,
        KEY_PAGEDOWN => 0x0100_0017,
        KEY_VOLUMEDOWN => 0x0100_0070,
        KEY_MUTE => 0x0100_0071,
        KEY_VOLUMEUP => 0x0100_0072,
        KEY_PLAYPAUSE => 0x0100_0080,
        KEY_STOPCD => 0x0100_0081,
        KEY_PREVIOUSSONG => 0x0100_0082,
        KEY_NEXTSONG => 0x0100_0083,
        KEY_BRIGHTNESSUP => 0x0100_00f2,
        KEY_BRIGHTNESSDOWN => 0x0100_00f3,
        KEY_0 => 0x30,
        r if (KEY_1..KEY_0).contains(&r) => 0x31 + (r - KEY_1) as i32,
        r if (KEY_F1..=KEY_F10).contains(&r) => 0x0100_0030 + (r - KEY_F1) as i32,
        KEY_F11 => 0x0100_003a,
        KEY_F12 => 0x0100_003b,
        _ => return None,
    };
    let mut code = key;
    if m.logo {
        code |= 0x1000_0000;
    }
    if m.alt {
        code |= 0x0800_0000;
    }
    if m.ctrl {
        code |= 0x0400_0000;
    }
    if m.shift {
        code |= 0x0200_0000;
    }
    Some(code)
}

/// Plasma: the global shortcut on this key, if one of the desktop's own (KWin's window
/// management, the launcher, the session's lock and log out, media and brightness, the panel's
/// task manager): (component, action).
pub fn plasma_action(raw: u32, m: Mods) -> Option<(String, String)> {
    let code = qt_key(raw, m)?;
    let out = Command::new("busctl")
        .args(["--user", "call", "org.kde.kglobalaccel", "/kglobalaccel", "org.kde.KGlobalAccel", "getGlobalShortcutsByKey", "i", &code.to_string()])
        .output()
        .ok()?;
    // a(ssssssaiai) N "action" "friendly" "component" …: the quoted strings, in order.
    let text = String::from_utf8_lossy(&out.stdout);
    let quoted: Vec<&str> = text.split('"').skip(1).step_by(2).collect();
    let (action, component) = (quoted.first()?.to_string(), quoted.get(2)?.to_string());
    const DESKTOP: [&str; 8] = ["kwin", "krunner", "org_kde_krunner_desktop", "ksmserver", "kmix", "mediacontrol", "org_kde_powerdevil", "plasmashell"];
    DESKTOP.contains(&component.as_str()).then_some((component, action))
}

/// Runs one of Plasma's global shortcuts, as its key would.
pub fn plasma_invoke(component: &str, action: &str) {
    let (path, action) = (format!("/component/{}", component.replace('.', "_")), action.to_string());
    std::thread::spawn(move || {
        let _ = crate::desktop::run_quiet(Command::new("busctl").args([
            "--user", "call", "org.kde.kglobalaccel", &path, "org.kde.kglobalaccel.Component", "invokeShortcut", "s", &action,
        ]));
    });
}

/// GNOME lets no app run its shortcuts (switcher, overview, workspaces), and while a Mac window
/// holds the keyboard it gets none of them. Its window keys are let through instead from that
/// press on, so the next one works: (with Super still held) the switcher on Tab, a workspace.
pub fn gnome_uses(raw: u32, m: Mods) -> bool {
    is_media(raw)
        || (raw == KEY_TAB && (m.logo || m.alt))
        || (matches!(raw, KEY_PAGEUP | KEY_PAGEDOWN | KEY_HOME | KEY_END) && m.logo)
        || (matches!(raw, KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT) && m.ctrl && m.alt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_text_navigation_are_the_apps_window_keys_the_desktops() {
        let cmd = Mods { logo: true, ..Default::default() };
        let opt = Mods { alt: true, ..Default::default() };
        // Cmd+C (KEY_C = 46), Cmd+T (20): the Mac's.
        assert!(!is_desktop_key(46, cmd));
        assert!(!is_desktop_key(20, cmd));
        // Cmd+Left, Opt+Right: through text.
        assert!(!is_desktop_key(KEY_LEFT, cmd));
        assert!(!is_desktop_key(KEY_RIGHT, opt));
        // Cmd+Tab, Alt+Tab, Cmd+Space, Cmd+1, Ctrl+Alt+Left, the volume key.
        assert!(is_desktop_key(KEY_TAB, cmd));
        assert!(is_desktop_key(KEY_TAB, opt));
        assert!(is_desktop_key(KEY_SPACE, cmd));
        assert!(is_desktop_key(KEY_1, cmd));
        assert!(is_desktop_key(KEY_LEFT, Mods { ctrl: true, alt: true, ..Default::default() }));
        assert!(is_desktop_key(KEY_VOLUMEUP, Mods::default()));
        // Plain Tab, Ctrl+Tab: the app's.
        assert!(!is_desktop_key(KEY_TAB, Mods::default()));
        assert!(!is_desktop_key(KEY_TAB, Mods { ctrl: true, ..Default::default() }));
    }

    #[test]
    fn keys_as_qt_counts_them() {
        assert_eq!(qt_key(KEY_TAB, Mods { alt: true, ..Default::default() }), Some(0x0800_0000 | 0x0100_0001));
        assert_eq!(qt_key(KEY_1, Mods { logo: true, ..Default::default() }), Some(0x1000_0000 | 0x31));
        assert_eq!(qt_key(KEY_0, Mods::default()), Some(0x30));
        assert_eq!(qt_key(KEY_F12, Mods::default()), Some(0x0100_003b));
    }
}
