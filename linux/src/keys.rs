//! The keys the desktop has now for Sweeflow's shortcuts (open, pause or resume, stop or
//! start), for the tray menu and the panels to show: `sweeflow install` sets Cmd+Opt+O / Y / Z
//! (setup.rs, hyprland.rs), and the user may have changed or removed them since in the
//! desktop's own settings. Read off the caller's thread, at most every few seconds.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// A key with its modifiers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Key {
    pub super_: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The key itself: a letter in capitals ("P"), or a name ("F5", "Escape").
    pub key: String,
}

impl Key {
    fn new(super_: bool, ctrl: bool, alt: bool, shift: bool, key: &str) -> Option<Key> {
        let key = match key.chars().count() {
            0 => return None,
            1 => key.to_uppercase(),
            _ => {
                let mut c = key.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        };
        Some(Key { super_, ctrl, alt, shift, key })
    }

    /// Cmd+Opt+<letter> as the desktop sees it: Super+Alt, or Ctrl+Alt behind a Mac-style key
    /// remapper (setup::mac_keys_remapped).
    pub fn cmd_opt(letter: char, remapped: bool) -> Key {
        Key { super_: !remapped, ctrl: remapped, alt: true, shift: false, key: letter.to_ascii_uppercase().to_string() }
    }

    /// As GNOME writes one: "<Super><Alt>p".
    pub fn from_gnome(text: &str) -> Option<Key> {
        let (mods, key) = crate::setup::gnome_key(text)?;
        let has = |m: &str| mods.iter().any(|x| x == m);
        Key::new(has("super"), has("ctrl"), has("alt"), has("shift"), &key)
    }

    /// As Qt counts one (Qt::META | Qt::ALT | Qt::Key_P): how KGlobalAccel gives them.
    pub fn from_qt(code: i32) -> Option<Key> {
        let key = code & 0x01ff_ffff;
        let name = match key {
            0x21..=0x7e => char::from(key as u8).to_string(),
            0x0100_0030..=0x0100_0052 => format!("F{}", key - 0x0100_0030 + 1),
            0x0100_0000 => "Escape".into(),
            0x0100_0001 => "Tab".into(),
            0x0100_0003 => "Backspace".into(),
            0x0100_0004 | 0x0100_0005 => "Return".into(),
            0x20 => "Space".into(),
            _ => return None,
        };
        Key::new(code & 0x1000_0000 != 0, code & 0x0400_0000 != 0, code & 0x0800_0000 != 0, code & 0x0200_0000 != 0, &name)
    }

    /// As Plasma's config writes one ("Meta+Alt+O"; the first of several, a tab or comma apart).
    pub fn from_qt_text(text: &str) -> Option<Key> {
        let first = text.split(['\t', ',']).next()?.trim();
        if first.is_empty() || first == "none" {
            return None;
        }
        let (mods, key) = match first.rsplit_once('+') {
            // "Ctrl++": the plus key.
            Some((mods, "")) => (mods.trim_end_matches('+'), "+"),
            Some((mods, key)) => (mods, key),
            None => ("", first),
        };
        let has = |m: &str| mods.split('+').any(|x| x.eq_ignore_ascii_case(m));
        Key::new(has("Meta"), has("Ctrl"), has("Alt"), has("Shift"), key)
    }

    /// As Hyprland lists one (`hyprctl binds -j`: its modmask, SHIFT 1, CTRL 4, ALT 8, SUPER 64,
    /// and key).
    pub fn from_hyprland(mask: u32, key: &str) -> Option<Key> {
        Key::new(mask & 64 != 0, mask & 4 != 0, mask & 8 != 0, mask & 1 != 0, key)
    }

    /// As a dbusmenu item's shortcut (the tray menu).
    pub fn menu(&self) -> Vec<Vec<String>> {
        let mods = [(self.ctrl, "Control"), (self.alt, "Alt"), (self.shift, "Shift"), (self.super_, "Super")];
        let mut keys: Vec<String> = mods.iter().filter(|m| m.0).map(|m| m.1.to_string()).collect();
        keys.push(self.key.clone());
        vec![keys]
    }

    /// As the panels show it: ⌘⌥P for Cmd+Opt+P (Super+Alt, or Ctrl+Alt behind a Mac-style key
    /// remapper, which is the same keys pressed), otherwise as the desktop writes it
    /// ("Ctrl+Alt+P"; `super_name`: "Meta" on Plasma, "Super" elsewhere).
    pub fn shown(&self, remapped: bool, super_name: &str) -> String {
        if self.alt && !self.shift && ((self.super_ && !self.ctrl) || (remapped && self.ctrl && !self.super_)) {
            return format!("⌘⌥{}", self.key);
        }
        let mods = [(self.super_, super_name), (self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")];
        let mut parts: Vec<&str> = mods.iter().filter(|m| m.0).map(|m| m.1).collect();
        parts.push(&self.key);
        parts.join("+")
    }
}

/// Sweeflow's keys now; None for one the desktop has no key for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keys {
    pub open: Option<Key>,
    pub pause: Option<Key>,
    pub stop: Option<Key>,
}

impl Keys {
    /// What `sweeflow install` sets (Cmd+Opt+O / Y / Z): shown until the desktop has been asked.
    fn defaults(remapped: bool) -> Keys {
        Keys { open: Some(Key::cmd_opt('O', remapped)), pause: Some(Key::cmd_opt('Y', remapped)), stop: Some(Key::cmd_opt('Z', remapped)) }
    }

    /// For the panel state (tray::panel_state): each as the panels show it, "" for none. The
    /// Global menu's key is fixed (caught in Mac windows only, ui.rs).
    pub fn json(&self) -> serde_json::Value {
        let remapped = crate::setup::mac_keys_remapped();
        let super_name = if desktop().contains("KDE") { "Meta" } else { "Super" };
        let shown = |k: &Option<Key>| k.as_ref().map(|k| k.shown(remapped, super_name)).unwrap_or_default();
        serde_json::json!({ "open": shown(&self.open), "pause": shown(&self.pause), "stop": shown(&self.stop), "menu": "⌘⌥U" })
    }
}

fn desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

/// Asks the desktop (slow: other processes, D-Bus calls).
fn read() -> Keys {
    let desktop = desktop();
    let remapped = crate::setup::mac_keys_remapped();
    if desktop.contains("KDE") {
        let [open, pause, stop] = crate::setup::plasma_bindings();
        Keys { open, pause, stop }
    } else if desktop.contains("Cinnamon") {
        let [open, pause, stop] = crate::setup::cinnamon_bindings().map(|k| k.as_deref().and_then(Key::from_gnome));
        Keys { open, pause, stop }
    } else if desktop.contains("GNOME") {
        let [open, pause, stop] = crate::setup::gnome_bindings().map(|k| k.as_deref().and_then(Key::from_gnome));
        Keys { open, pause, stop }
    } else if crate::hyprland::running() {
        let [open, pause, stop] = crate::hyprland::our_keys().map(|k| k.and_then(|(m, k)| Key::from_hyprland(m, &k)));
        // The open key is bound whatever else has it (hyprland.rs), by a version that didn't
        // describe it yet until Hyprland's config is reloaded.
        Keys { open: open.or(Some(Key::cmd_opt('O', remapped))), pause, stop }
    } else {
        // Nothing set on this desktop: the keys work in the Mac windows (ui.rs).
        Keys::defaults(false)
    }
}

static CACHE: Mutex<Option<(Instant, Keys)>> = Mutex::new(None);
static READING: AtomicBool = AtomicBool::new(false);
static AGAIN: AtomicBool = AtomicBool::new(false);
const FRESH: Duration = Duration::from_secs(5);

/// The keys as last read (the defaults before the first read), read again off this thread once
/// they are a few seconds old; when they changed, the tray and panels are refreshed.
pub fn current() -> Keys {
    let cache = CACHE.lock().unwrap();
    let keys = cache.as_ref().map(|c| c.1.clone()).unwrap_or_else(|| Keys::defaults(crate::setup::mac_keys_remapped()));
    let fresh = cache.as_ref().is_some_and(|c| c.0.elapsed() < FRESH);
    drop(cache);
    if !fresh {
        reread();
    }
    keys
}

/// Reads the keys again, off this thread (unless a read is under way already); when they
/// changed, the tray and panels are refreshed. Also what Sweeflow's own bindings call once
/// they're added (hyprland.rs).
pub fn reread() {
    // A read under way may have asked before the change: it reads once more after.
    AGAIN.store(true, Ordering::SeqCst);
    if READING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        while AGAIN.swap(false, Ordering::SeqCst) {
            let now = read();
            let changed = CACHE.lock().unwrap().replace((Instant::now(), now.clone())).is_none_or(|(_, before)| before != now);
            if changed {
                crate::tray::refresh();
            }
        }
        READING.store(false, Ordering::SeqCst);
        // (One asked for between the last look and here starts its own read.)
        if AGAIN.load(Ordering::SeqCst) {
            reread();
        }
    });
}

/// Keeps the keys shown up to date while the client runs: the user changes them in the
/// desktop's settings, which says nothing to Sweeflow. A look every so often.
pub fn watch() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(10));
        reread();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_as_each_desktop_gives_them() {
        let p = Key::cmd_opt('p', false);
        assert_eq!(Key::from_gnome("<Super><Alt>p"), Some(p.clone()));
        assert_eq!(Key::from_gnome("<Mod4><Mod1>P"), Some(p.clone()));
        assert_eq!(Key::from_qt(0x1800_0050), Some(p.clone()));
        assert_eq!(Key::from_qt_text("Meta+Alt+P"), Some(p.clone()));
        assert_eq!(Key::from_qt_text("Meta+Alt+P\tCtrl+X"), Some(p.clone()));
        assert_eq!(Key::from_hyprland(72, "P"), Some(p.clone()));
        let remapped = Key::cmd_opt('X', true);
        assert_eq!(Key::from_gnome("<Control><Alt>x"), Some(remapped.clone()));
        assert_eq!(Key::from_qt(0x0c00_0058), Some(remapped.clone()));
        assert_eq!(Key::from_qt_text("Ctrl+Alt+X"), Some(remapped.clone()));
        assert_eq!(Key::from_hyprland(12, "x"), Some(remapped));
        assert_eq!(Key::from_qt(0x0400_0000 | 0x0100_0034).map(|k| k.key), Some("F5".to_string()));
        assert_eq!(Key::from_gnome("<Primary><Shift>escape").map(|k| (k.ctrl, k.shift, k.key)), Some((true, true, "Escape".to_string())));
        assert_eq!(Key::from_qt_text("Ctrl++").map(|k| (k.ctrl, k.key)), Some((true, "+".to_string())));
        assert_eq!(Key::from_qt_text("none"), None);
        assert_eq!(Key::from_qt_text(""), None);
        assert_eq!(Key::from_gnome(""), None);
        assert_eq!(Key::from_qt(0), None);
    }

    #[test]
    fn shown_as_mac_keys_when_they_are() {
        let p = Key::cmd_opt('P', false);
        assert_eq!(p.shown(false, "Super"), "⌘⌥P");
        assert_eq!(p.menu(), [["Alt", "Super", "P"]]);
        // Behind a key remapper Ctrl+Alt is what Cmd+Opt arrives as.
        let ctrl = Key::cmd_opt('P', true);
        assert_eq!(ctrl.shown(true, "Super"), "⌘⌥P");
        assert_eq!(ctrl.shown(false, "Super"), "Ctrl+Alt+P");
        assert_eq!(ctrl.menu(), [["Control", "Alt", "P"]]);
        // Others as the desktop writes them.
        let other = Key::from_qt_text("Meta+Shift+P").unwrap();
        assert_eq!(other.shown(false, "Meta"), "Meta+Shift+P");
        assert_eq!(Key::from_gnome("<Super>F5").unwrap().shown(false, "Super"), "Super+F5");
    }

    #[test]
    fn the_panels_get_each_key_or_none() {
        let keys = Keys { open: Some(Key::cmd_opt('O', false)), pause: None, stop: Key::from_qt_text("Meta+Ctrl+S") };
        let json = keys.json();
        assert_eq!(json["open"], "⌘⌥O");
        assert_eq!(json["pause"], "");
        assert!(json["stop"].as_str().unwrap().ends_with("Ctrl+S"));
        assert_eq!(json["menu"], "⌘⌥U");
    }
}
