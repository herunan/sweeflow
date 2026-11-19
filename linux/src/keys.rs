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

    /// As the panels show it: Super+Alt+P, with or without a key remapper (Super is the key Mac
    /// windows get as ⌘; behind a remapper it's set as Ctrl+Alt, the same key pressed); others as
    /// the desktop writes them ("Ctrl+Shift+P"; `super_name`: "Meta" on Plasma, "Super" elsewhere).
    pub fn shown(&self, remapped: bool, super_name: &str) -> String {
        if remapped && self.alt && !self.shift && self.ctrl && !self.super_ {
            return format!("Super+Alt+{}", self.key);
        }
        if self.alt && !self.shift && self.super_ && !self.ctrl {
            return format!("Super+Alt+{}", self.key);
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
        self.json_for(crate::setup::mac_keys_remapped())
    }

    /// As `json`, for a keyboard made Mac-style by a remapper (`remapped`) or not.
    fn json_for(&self, remapped: bool) -> serde_json::Value {
        own_json(remapped)
    }
}

pub(crate) fn desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

/// Asks the desktop (slow: other processes, D-Bus calls).
pub(crate) fn read() -> Keys {
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
        assert_eq!(p.shown(false, "Meta"), "Super+Alt+P");
        assert_eq!(p.menu(), [["Alt", "Super", "P"]]);
        // Behind a key remapper Ctrl+Alt is what Cmd+Opt arrives as.
        let ctrl = Key::cmd_opt('P', true);
        assert_eq!(ctrl.shown(true, "Super"), "Super+Alt+P");
        assert_eq!(p.shown(true, "Super"), "Super+Alt+P");
        assert_eq!(ctrl.shown(false, "Super"), "Ctrl+Alt+P");
        assert_eq!(ctrl.menu(), [["Control", "Alt", "P"]]);
        // Others as the desktop writes them.
        let other = Key::from_qt_text("Meta+Shift+P").unwrap();
        assert_eq!(other.shown(false, "Meta"), "Meta+Shift+P");
        assert_eq!(Key::from_gnome("<Super>F5").unwrap().shown(false, "Super"), "Super+F5");
    }

    #[test]
    fn the_panels_get_each_key_or_none() {
        // As set (none set: the defaults), the keyboard's way or the Mac's.
        let defaults = super::own_keys_from(&Default::default());
        let json = super::json_of(&defaults, false);
        assert_eq!(json["open"], "Super+Alt+O");
        assert_eq!(json["menu"], "Super+Alt+U");
        // A remapper changes nothing in how they're written: Super is the ⌘ key in Mac windows.
        assert_eq!(super::json_of(&defaults, true)["menu"], "Super+Alt+U");
        assert_eq!(super::json_of(&defaults, true)["remapped"], true);
        // One set to none, one changed.
        let set = [("pause".to_string(), String::new()), ("stop".to_string(), "Super+Shift+S".to_string())].into();
        let json = super::json_of(&super::own_keys_from(&set), false);
        assert_eq!(json["pause"], "");
        assert_eq!(json["stop"], "Super+Shift+S");
    }
}

/// `caps` for a Slint model (the keycaps a window shows).
pub fn caps_model(shown: &str) -> slint::ModelRc<slint::SharedString> {
    let caps: Vec<slint::SharedString> = caps(shown).into_iter().map(Into::into).collect();
    slint::ModelRc::new(slint::VecModel::from(caps))
}

/// A shortcut as shown ("Super+Alt+O", "⌘⌥O", "Ctrl++", "Volume +") split into its keys, one
/// keycap each: ["Super", "Alt", "O"], ["⌘", "⌥", "O"]. Empty: none.
pub fn caps(shown: &str) -> Vec<String> {
    let shown = shown.trim();
    if shown.is_empty() {
        return Vec::new();
    }
    if shown.contains('+') && shown.len() > 1 && !shown.contains(' ') {
        let mut parts: Vec<String> = shown.split('+').map(str::to_string).collect();
        // "Ctrl++": the last key is "+".
        if shown.ends_with("++") {
            parts.retain(|p| !p.is_empty());
            parts.push("+".into());
        }
        return parts.into_iter().filter(|p| !p.is_empty()).collect();
    }
    let signs: String = shown.chars().take_while(|c| "⌃⌥⇧⌘".contains(*c)).collect();
    if signs.is_empty() {
        return vec![shown.to_string()];
    }
    // Each sign with its name, as a Mac keyboard has them: the signs alone are hard to tell apart.
    let named = |c: char| match c {
        '⌘' => "⌘ Cmd".to_string(),
        '⌥' => "⌥ Opt".into(),
        '⌃' => "⌃ Ctrl".into(),
        '⇧' => "⇧ Shift".into(),
        other => other.to_string(),
    };
    let mut out: Vec<String> = signs.chars().map(named).collect();
    let rest = shown[signs.len()..].trim();
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

#[cfg(test)]
mod caps_tests {
    #[test]
    fn shortcuts_split_into_keycaps() {
        assert_eq!(super::caps("Super+Alt+O"), ["Super", "Alt", "O"]);
        assert_eq!(super::caps("⌘⌥O"), ["⌘ Cmd", "⌥ Opt", "O"]);
        assert_eq!(super::caps("⌘Space"), ["⌘ Cmd", "Space"]);
        assert_eq!(super::caps("⌃⇧Tab"), ["⌃ Ctrl", "⇧ Shift", "Tab"]);
        assert_eq!(super::caps("Ctrl++"), ["Ctrl", "+"]);
        assert_eq!(super::caps("Volume +"), ["Volume +"]);
        assert!(super::caps("").is_empty());
    }
}

/// For the panels: Sweeflow's keys as set (keys.rs's own keys, which the desktop is given), as
/// the keyboard names them (Super+Alt+O; Super is ⌘ in Mac windows, a remapper or not); "" for
/// none.
pub fn own_json(remapped: bool) -> serde_json::Value {
    json_of(&own_keys(), remapped)
}

fn json_of(keys: &[(&'static str, Option<crate::deskkeys::UserKey>)], remapped: bool) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (name, key) in keys {
        out.insert(name.to_string(), key.as_ref().map(|k| k.label()).unwrap_or_default().into());
    }
    // A Mac-style key remapper is set up (its note says so; the keys are written the same).
    out.insert("remapped".into(), remapped.into());
    // Alt and Super swapped in Mac windows (config::swap_cmd_opt), for the mapping line.
    out.insert("swapped".into(), crate::config::swap_cmd_opt().into());
    serde_json::Value::Object(out)
}

/// Sweeflow's own keys, by name, with what they are unless set otherwise (in Mac terms: Super
/// is ⌘, as in Mac windows). The Global menu acts only in Mac windows.
pub const OWN: [(&str, &str); 4] = [("open", "Super+Alt+O"), ("menu", "Super+Alt+U"), ("pause", "Super+Alt+Y"), ("stop", "Super+Alt+Z")];

fn own_file() -> std::path::PathBuf {
    crate::config::dir().join("keys.json")
}

/// Sweeflow's keys as set in its Shortcuts window (`keys.json`: name → "Super+Alt+O", "" for
/// none), the defaults for the rest. Read again only when the file changed.
pub fn own_keys() -> Vec<(&'static str, Option<crate::deskkeys::UserKey>)> {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(Option<std::time::SystemTime>, Vec<(&'static str, Option<crate::deskkeys::UserKey>)>)>> = Mutex::new(None);
    let stamp = std::fs::metadata(own_file()).and_then(|m| m.modified()).ok();
    let mut cache = CACHE.lock().unwrap();
    if cache.as_ref().is_none_or(|(s, _)| *s != stamp) {
        let set: std::collections::HashMap<String, String> =
            serde_json::from_slice(&std::fs::read(own_file()).unwrap_or_default()).unwrap_or_default();
        *cache = Some((stamp, own_keys_from(&set)));
    }
    cache.as_ref().map(|(_, k)| k.clone()).unwrap_or_default()
}

/// Sweeflow's keys from what's set (name → "Super+Alt+O", "" for none), the defaults for the rest.
fn own_keys_from(set: &std::collections::HashMap<String, String>) -> Vec<(&'static str, Option<crate::deskkeys::UserKey>)> {
    OWN.iter().map(|(name, default)| (*name, crate::deskkeys::UserKey::parse(set.get(*name).map_or(default, String::as_str)))).collect()
}

/// Sweeflow's keys as set (keys.json), for the tray menu: what every place shows, so a key changed
/// in the Shortcuts window shows the same everywhere.
pub fn own_as_keys() -> Keys {
    let to_key = |name: &str| own_key(name).and_then(|k| Key::new(k.logo, k.ctrl, k.alt, k.shift, &k.key));
    Keys { open: to_key("open"), pause: to_key("pause"), stop: to_key("stop") }
}

/// One of Sweeflow's keys, as set.
pub fn own_key(name: &str) -> Option<crate::deskkeys::UserKey> {
    own_keys().into_iter().find(|(n, _)| *n == name).and_then(|(_, k)| k)
}

/// Sets one of Sweeflow's keys ("" for none).
pub fn set_own_key(name: &str, label: &str) -> std::io::Result<()> {
    let mut set: std::collections::HashMap<String, String> =
        serde_json::from_slice(&std::fs::read(own_file()).unwrap_or_default()).unwrap_or_default();
    set.insert(name.to_string(), label.to_string());
    std::fs::create_dir_all(crate::config::dir())?;
    std::fs::write(own_file(), serde_json::to_vec_pretty(&set).unwrap_or_default())
}

/// Which of Sweeflow's keys a press in a Mac window is, if any ("open", "menu", "pause", "stop").
/// The key code sent to the Mac for a key pressed in a Mac window: Alt and Super swapped when
/// that's set (config::swap_cmd_opt), so ⌘ is next to the space bar on a PC keyboard.
pub fn mac_code(raw: u32) -> u32 {
    if crate::config::swap_cmd_opt() { swapped(raw) } else { raw }
}

/// Left Alt and left Super, right Alt and right Super, the other way round.
fn swapped(raw: u32) -> u32 {
    match raw {
        56 => 125,
        125 => 56,
        100 => 126,
        126 => 100,
        other => other,
    }
}

/// Keys typed right after ⌘N wait for the window it opens. On the Mac the app takes them in
/// order, after ⌘N, into the new window; sent from here they reached it before the window was
/// there (it took half a second on the Mac to appear) and went into the old one. So after ⌘N
/// (with Shift or not) the keys that follow are held, in order, until a new window comes or
/// `HOLD_FOR_NEW_WINDOW` is up. The ⌘N's own keys let go meanwhile still go at once.
#[derive(Default)]
pub struct NewWindowWait {
    cmd_down: u8,
    until: Option<std::time::Instant>,
    held: Vec<(u32, u8)>,
}

pub const HOLD_FOR_NEW_WINDOW: std::time::Duration = std::time::Duration::from_millis(800);

impl NewWindowWait {
    /// A key on its way to the Mac (Mac-side code as mac_code gives it; 1 press, 0 let go, 2
    /// repeat): true when it goes now, false when it's held (taken: `release` gives it back).
    pub fn pass(&mut self, code: u32, state: u8) -> bool {
        let is_cmd = code == 125 || code == 126;
        if self.until.is_some_and(|t| std::time::Instant::now() >= t) {
            self.until = None;
        }
        let holding = self.until.is_some();
        // Keys let go before anything was held: the ⌘N's own.
        let goes = !holding || (state == 0 && self.held.is_empty());
        if goes {
            if is_cmd {
                match state {
                    1 => self.cmd_down = self.cmd_down.saturating_add(1),
                    0 => self.cmd_down = self.cmd_down.saturating_sub(1),
                    _ => {}
                }
            }
            // ⌘N pressed: what comes next waits for its window.
            if code == 49 && state == 1 && self.cmd_down > 0 {
                self.until = Some(std::time::Instant::now() + HOLD_FOR_NEW_WINDOW);
            }
            return true;
        }
        self.held.push((code, state));
        false
    }

    /// Waiting for a new window (a timer should call `release` when it's up).
    pub fn waiting(&self) -> bool {
        self.until.is_some()
    }

    /// The new window came (or the wait is up): the keys held, in order, to send now.
    pub fn release(&mut self) -> Vec<(u32, u8)> {
        self.until = None;
        let held = std::mem::take(&mut self.held);
        for &(code, state) in &held {
            if code == 125 || code == 126 {
                match state {
                    1 => self.cmd_down = self.cmd_down.saturating_add(1),
                    0 => self.cmd_down = self.cmd_down.saturating_sub(1),
                    _ => {}
                }
            }
        }
        held
    }
}

#[cfg(test)]
#[test]
fn keys_after_cmd_n_wait_for_the_new_window() {
    let mut w = NewWindowWait::default();
    // ⌘ down, N down: both go.
    assert!(w.pass(125, 1) && w.pass(49, 1));
    assert!(w.waiting());
    // N and ⌘ let go before anything is typed: go too (the shortcut's own).
    assert!(w.pass(49, 0));
    // Typing: held, and what's let go after it too (order kept).
    assert!(!w.pass(45, 1));
    assert!(!w.pass(125, 0));
    assert!(!w.pass(45, 0));
    assert_eq!(w.release(), vec![(45, 1), (125, 0), (45, 0)]);
    assert!(!w.waiting());
    // ⌘ is up now: N alone doesn't wait.
    assert!(w.pass(49, 1) && w.pass(49, 0));
    assert!(!w.waiting());
}

#[cfg(test)]
#[test]
fn swap_turns_alt_into_cmd_and_super_into_opt() {
    // evdev codes: left Alt 56, left Super 125, right Alt 100, right Super 126.
    assert_eq!(swapped(56), 125);
    assert_eq!(swapped(125), 56);
    assert_eq!(swapped(100), 126);
    assert_eq!(swapped(126), 100);
    // Everything else as it is: Ctrl, Shift, letters.
    for code in [29, 42, 30, 24, 57] {
        assert_eq!(swapped(code), code);
    }
}

pub fn own_action(raw: u32, ch: Option<char>, sym_name: Option<&str>, m: crate::deskkeys::Mods) -> Option<&'static str> {
    action_in(&own_keys(), raw, ch, sym_name, m)
}

fn action_in(keys: &[(&'static str, Option<crate::deskkeys::UserKey>)], raw: u32, ch: Option<char>, sym_name: Option<&str>, m: crate::deskkeys::Mods) -> Option<&'static str> {
    keys.iter().find(|(_, k)| k.as_ref().is_some_and(|k| k.matches(raw, ch, sym_name, m))).map(|(n, _)| *n)
}

#[cfg(test)]
mod own_tests {
    use crate::deskkeys::Mods;

    #[test]
    fn mac_windows_follow_the_keys_as_set() {
        let cmd_opt = Mods { logo: true, alt: true, ..Default::default() };
        let keys = super::own_keys_from(&Default::default());
        // The defaults, by what the key types (any layout, Caps Lock or not).
        assert_eq!(super::action_in(&keys, 24, Some('u'), Some("XK_u"), cmd_opt), Some("menu"));
        assert_eq!(super::action_in(&keys, 24, Some('U'), Some("XK_U"), cmd_opt), Some("menu"));
        assert_eq!(super::action_in(&keys, 24, Some('o'), Some("XK_o"), cmd_opt), Some("open"));
        assert_eq!(super::action_in(&keys, 21, Some('y'), Some("XK_y"), cmd_opt), Some("pause"));
        assert_eq!(super::action_in(&keys, 44, Some('z'), Some("XK_z"), cmd_opt), Some("stop"));
        // Other modifiers held: not Sweeflow's.
        assert_eq!(super::action_in(&keys, 24, Some('u'), Some("XK_u"), Mods { shift: true, ..cmd_opt }), None);
        assert_eq!(super::action_in(&keys, 24, Some('u'), Some("XK_u"), Mods { logo: true, ..Default::default() }), None);
    }
}
