//! Which keys stay the desktop's while a Mac window has the keyboard and the pointer is over a
//! Mac window (then every other shortcut goes to the Mac, Super as Cmd). The keys: one list, the
//! same on every desktop (keptkeys.rs), a superset of the standard defaults of Plasma, GNOME,
//! Cinnamon and Omarchy for switching, workspaces and windows, plus the media keys; shortcuts
//! with a letter are the app's (Cmd+C, Cmd+T, Opt+letter for accents), and so are Cmd or Opt
//! with arrows (moving through text). The person can take keys out of the list and add their
//! own (the Shortcuts window, linuxkeys.rs). Of those, only the ones the desktop actually uses
//! are taken from the Mac where that can be known (Plasma says; X11 gets the key back and passes
//! on what it doesn't use; GNOME runs none for an app, so its window keys are let through from
//! the next press). Not on Hyprland, where Sweeflow's key mode binds them (hyprland::mac_keys).

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
    crate::keptkeys::is_kept(raw, m) && !press_removed(raw, m)
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
    plasma_lookup(qt_key(raw, m)?, true)
}

/// The global shortcut Plasma has on a key (Qt's number): (component, action). `desktop_only`:
/// only the desktop's own components.
fn plasma_lookup(code: i32, desktop_only: bool) -> Option<(String, String)> {
    let out = Command::new("busctl")
        .args(["--user", "call", "org.kde.kglobalaccel", "/kglobalaccel", "org.kde.KGlobalAccel", "getGlobalShortcutsByKey", "i", &code.to_string()])
        .output()
        .ok()?;
    // a(ssssssaiai) N "action" "friendly" "component" …: the quoted strings, in order.
    let text = String::from_utf8_lossy(&out.stdout);
    let quoted: Vec<&str> = text.split('"').skip(1).step_by(2).collect();
    let (action, component) = (quoted.first()?.to_string(), quoted.get(2)?.to_string());
    const DESKTOP: [&str; 8] = ["kwin", "krunner", "org_kde_krunner_desktop", "ksmserver", "kmix", "mediacontrol", "org_kde_powerdevil", "plasmashell"];
    (!desktop_only || DESKTOP.contains(&component.as_str())).then_some((component, action))
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

/// A key the person keeps for Linux even over a focused Mac window ("Super+E"), on top of the
/// ones above: set in the Exceptions window (linuxkeys.rs), saved in `linux-keys.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct UserKey {
    pub logo: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    /// The key's name: a letter, digit or sign in capitals ("E", "1", "/"), or one of KEYS.
    pub key: String,
}

/// The keys with names, as written ("PageUp") and as xkb calls them ("Prior").
const KEYS: [(&str, &str); 19] = [
    ("Space", "space"), ("Enter", "Return"), ("Esc", "Escape"), ("Tab", "Tab"), ("Backspace", "BackSpace"),
    ("Delete", "Delete"), ("Insert", "Insert"), ("Home", "Home"), ("End", "End"), ("PageUp", "Prior"),
    ("PageDown", "Next"), ("Left", "Left"), ("Right", "Right"), ("Up", "Up"), ("Down", "Down"), ("Print", "Print"),
    ("Pause", "Pause"), ("Menu", "Menu"), ("ScrollLock", "Scroll_Lock"),
];

impl UserKey {
    /// "Super+Shift+E", "ctrl+alt+t", "Meta+F5", "Cmd+Space": None when it isn't a key.
    pub fn parse(text: &str) -> Option<UserKey> {
        let mut k = UserKey { logo: false, alt: false, ctrl: false, shift: false, key: String::new() };
        // The Mac's way: "⌘E", "⌃⌥T" (signs, then the key).
        let text = text.trim();
        if !text.contains('+') && text.starts_with(['⌘', '⌃', '⌥', '⇧']) {
            let rest = text.trim_start_matches(['⌘', '⌃', '⌥', '⇧']);
            for c in text[..text.len() - rest.len()].chars() {
                match c {
                    '⌘' => k.logo = true,
                    '⌃' => k.ctrl = true,
                    '⌥' => k.alt = true,
                    _ => k.shift = true,
                }
            }
            k.key = Self::name(rest.trim())?;
            return Some(k);
        }
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        // "Ctrl++": the key is "+".
        let (mods, key) = match parts.as_slice() {
            [mods @ .., "", ""] if !mods.is_empty() => (mods, "+"),
            [mods @ .., key] => (mods, *key),
            [] => return None,
        };
        for m in mods {
            match m.to_lowercase().as_str() {
                "super" | "meta" | "cmd" | "command" | "win" | "windows" | "logo" | "⌘" => k.logo = true,
                "alt" | "opt" | "option" | "⌥" => k.alt = true,
                "ctrl" | "control" | "⌃" => k.ctrl = true,
                "shift" | "⇧" => k.shift = true,
                _ => return None,
            }
        }
        k.key = Self::name(key)?;
        Some(k)
    }

    /// A key as written: one character in capitals, or one of KEYS (or F1–F24), any case.
    fn name(key: &str) -> Option<String> {
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return (!c.is_whitespace() && !c.is_control()).then(|| c.to_uppercase().collect());
        }
        let lower = key.to_lowercase();
        let alias = match lower.as_str() {
            "return" => "enter",
            "escape" => "esc",
            "del" => "delete",
            "pgup" => "pageup",
            "pgdn" | "pgdown" => "pagedown",
            other => other,
        };
        if let Some((name, _)) = KEYS.iter().find(|(n, _)| n.to_lowercase() == alias) {
            return Some(name.to_string());
        }
        let f = alias.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=24).contains(n));
        f.map(|n| format!("F{n}"))
    }

    /// As shown and saved: "Super+Ctrl+Alt+Shift+E", in that order.
    pub fn label(&self) -> String {
        let mods = [(self.logo, "Super"), (self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")];
        let mut parts: Vec<&str> = mods.iter().filter(|m| m.0).map(|m| m.1).collect();
        parts.push(&self.key);
        parts.join("+")
    }

    /// In Mac signs, in the order of `label` (so [Super][Alt][O] and [⌘][⌥][O] line up): "⌘⌥O".
    pub fn mac_label(&self) -> String {
        let signs = [(self.logo, "⌘"), (self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧")];
        let mut out: String = signs.iter().filter(|s| s.0).map(|s| s.1).collect();
        out.push_str(&self.key);
        out
    }

    /// Whether a press is this key: by what it types (its symbol's character, so it follows the
    /// keyboard's layout) or its symbol's xkb name; digits by their place (Shift+1 types "!").
    pub fn matches(&self, raw: u32, ch: Option<char>, sym_name: Option<&str>, m: Mods) -> bool {
        if (self.logo, self.alt, self.ctrl, self.shift) != (m.logo, m.alt, m.ctrl, m.shift) {
            return false;
        }
        if (KEY_1..=KEY_0).contains(&raw) {
            let digit = if raw == KEY_0 { 0 } else { raw - KEY_1 + 1 };
            return self.key == digit.to_string();
        }
        let xkb = sym_name.map(|n| n.trim_start_matches("XK_"));
        if let Some(xkb) = xkb {
            if KEYS.iter().any(|(name, x)| *x == xkb && *name == self.key) || (xkb.starts_with('F') && xkb == self.key) {
                return true;
            }
        }
        ch.filter(|c| !c.is_control() && !c.is_whitespace()).is_some_and(|c| c.to_uppercase().collect::<String>() == self.key)
    }

    /// Qt's number for it, as Plasma's global shortcuts are kept.
    pub fn qt(&self) -> Option<i32> {
        let mut shifted = false;
        let key = match self.key.as_str() {
            "Space" => 0x20,
            "Enter" => 0x0100_0004,
            "Esc" => 0x0100_0000,
            "Tab" => 0x0100_0001,
            "Backspace" => 0x0100_0003,
            "Delete" => 0x0100_0007,
            "Insert" => 0x0100_0006,
            "Home" => 0x0100_0010,
            "End" => 0x0100_0011,
            "Left" => 0x0100_0012,
            "Up" => 0x0100_0013,
            "Right" => 0x0100_0014,
            "Down" => 0x0100_0015,
            "PageUp" => 0x0100_0016,
            "PageDown" => 0x0100_0017,
            "Print" => 0x0100_0009,
            "Pause" => 0x0100_0008,
            "Menu" => 0x0100_0055,
            "ScrollLock" => 0x0100_0026,
            f if f.starts_with('F') && f.len() > 1 => 0x0100_0030 + f[1..].parse::<i32>().ok()? - 1,
            c => {
                let c = c.chars().next()?;
                if !c.is_ascii() {
                    return None;
                }
                // With Shift, a digit or sign is the sign Shift makes (a US layout), and Shift
                // goes: that's how Plasma writes and matches them (Meta+! for Super+Shift+1);
                // Ctrl+Shift+; is never pressed, Ctrl+: is.
                if self.shift {
                    if let Some(at) = "1234567890-=[]\\;',./`".find(c) {
                        shifted = true;
                        "!@#$%^&*()_+{}|:\"<>?~".as_bytes()[at] as i32
                    } else {
                        c.to_ascii_uppercase() as i32
                    }
                } else {
                    c.to_ascii_uppercase() as i32
                }
            }
        };
        let mut code = key;
        for (on, bit) in [(self.logo, 0x1000_0000), (self.alt, 0x0800_0000), (self.ctrl, 0x0400_0000), (self.shift && !shifted, 0x0200_0000)] {
            if on {
                code |= bit;
            }
        }
        Some(code)
    }
}

fn user_keys_file() -> std::path::PathBuf {
    crate::config::dir().join("linux-keys.json")
}

/// The keys kept for Linux, as saved (labels).
/// What the Shortcuts window keeps in `linux-keys.json`: the person's own keys ("Super+E") and
/// the built-in ones they took out, by name ("Super+Tab", keptkeys.rs). An earlier version kept only a list
/// of own keys.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Exceptions {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

pub fn load_exceptions() -> Exceptions {
    let bytes = std::fs::read(user_keys_file()).unwrap_or_default();
    serde_json::from_slice(&bytes)
        .or_else(|_| serde_json::from_slice::<Vec<String>>(&bytes).map(|added| Exceptions { added, removed: Vec::new() }))
        .unwrap_or_default()
}

pub fn save_exceptions(e: &Exceptions) -> std::io::Result<()> {
    std::fs::create_dir_all(crate::config::dir())?;
    std::fs::write(user_keys_file(), serde_json::to_vec_pretty(e).unwrap_or_default())
}

/// The person's own keys for Linux, as saved.
pub fn load_user_keys() -> Vec<String> {
    load_exceptions().added
}

/// The exceptions as now, parsed, read again only when the file changed.
fn exceptions_now() -> (Vec<UserKey>, Vec<String>) {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(Option<std::time::SystemTime>, Vec<UserKey>, Vec<String>)>> = Mutex::new(None);
    let stamp = std::fs::metadata(user_keys_file()).and_then(|m| m.modified()).ok();
    let mut cache = CACHE.lock().unwrap();
    if cache.as_ref().is_none_or(|(s, _, _)| *s != stamp) {
        let e = load_exceptions();
        let keys = e.added.iter().filter_map(|k| UserKey::parse(k)).collect();
        *cache = Some((stamp, keys, e.removed));
    }
    let (_, keys, removed) = cache.as_ref().unwrap();
    (keys.clone(), removed.clone())
}

/// The press is one of the person's own keys for Linux.
pub fn is_user_key(raw: u32, ch: Option<char>, sym_name: Option<&str>, m: Mods) -> Option<UserKey> {
    exceptions_now().0.into_iter().find(|k| k.matches(raw, ch, sym_name, m))
}

/// A built-in key the person took out (in the Shortcuts window), by name ("Super+Tab",
/// keptkeys::press_label).
pub fn key_removed(label: &str) -> bool {
    exceptions_now().1.iter().any(|k| k == label)
}

/// A press of a built-in key the person took out.
pub fn press_removed(raw: u32, m: Mods) -> bool {
    crate::keptkeys::press_label(raw, m).is_some_and(|l| key_removed(&l))
}

/// Plasma: whatever global shortcut is on this key (any app's too, not only the desktop's):
/// (component, action).
pub fn plasma_action_any(qt: i32) -> Option<(String, String)> {
    plasma_lookup(qt, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_keys_are_read_written_and_matched() {
        let k = UserKey::parse("super+shift+e").unwrap();
        assert_eq!(k.label(), "Super+Shift+E");
        assert_eq!(UserKey::parse("Meta+Ctrl+Alt+pgup").unwrap().label(), "Super+Ctrl+Alt+PageUp");
        assert_eq!(UserKey::parse("Ctrl++").unwrap().label(), "Ctrl++");
        assert_eq!(UserKey::parse("Cmd+F5").unwrap().label(), "Super+F5");
        assert!(UserKey::parse("Hyper+E").is_none());
        assert!(UserKey::parse("Super+Banana").is_none());
        assert_eq!(UserKey::parse("⌃⌥T").unwrap().label(), "Ctrl+Alt+T");
        assert_eq!(UserKey::parse("⌘E").unwrap().mac_label(), "⌘E");

        let m = Mods { logo: true, shift: true, ..Default::default() };
        // By what the key types (Shift makes it "E"), whatever the layout.
        assert!(k.matches(18, Some('E'), Some("XK_E"), m));
        assert!(!k.matches(18, Some('E'), Some("XK_E"), Mods { logo: true, ..Default::default() }));
        // Digits by their place: Shift+1 types "!".
        let one = UserKey::parse("Super+Shift+1").unwrap();
        assert!(one.matches(KEY_1, Some('!'), Some("XK_exclam"), m));
        // Named keys by their xkb name.
        let up = UserKey::parse("Super+PageUp").unwrap();
        assert!(up.matches(KEY_PAGEUP, None, Some("XK_Prior"), Mods { logo: true, ..Default::default() }));
        let f5 = UserKey::parse("Super+F5").unwrap();
        assert!(f5.matches(63, None, Some("XK_F5"), Mods { logo: true, ..Default::default() }));

        // Plasma's numbers: Meta 0x10000000, Shift 0x02000000, the letter's capital.
        assert_eq!(k.qt(), Some(0x1000_0000 | 0x0200_0000 | 'E' as i32));
        assert_eq!(f5.qt(), Some(0x1000_0000 | 0x0100_0034));
        // Shift with a digit or sign: the sign Shift makes, without Shift (as Plasma has them).
        assert_eq!(UserKey::parse("Ctrl+Shift+;").unwrap().qt(), Some(0x0400_0000 | ':' as i32));
        assert_eq!(UserKey::parse("Super+Shift+1").unwrap().qt(), Some(0x1000_0000 | '!' as i32));
    }

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
        // Cmd+Tab, Alt+Tab, the volume key: kept for Linux by default (keptkeys.rs; the person's
        // own removals, in their config, aren't this test's business).
        assert!(crate::keptkeys::is_kept(KEY_TAB, cmd));
        assert!(crate::keptkeys::is_kept(KEY_TAB, opt));
        assert!(crate::keptkeys::is_kept(KEY_VOLUMEUP, Mods::default()));
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
