//! The keys that stay Linux's in Mac windows, one by one: a fixed superset of the standard
//! default shortcuts of Plasma, GNOME, Cinnamon and Omarchy for switching, workspaces and
//! windows, plus the media keys. The same list on every desktop (a key a desktop doesn't use
//! goes back to the Mac window, deskkeys.rs), shown in the Shortcuts window (linuxkeys.rs), where
//! the person can take any out. A key is named the way it's saved when taken out ("Super+Tab",
//! "Volume Up"), the same as a press is named (`press_label`), so the two can be compared.

use crate::deskkeys::Mods;

/// One key kept for Linux: its name ("Super+Tab"), what the desktop does with it, and the
/// heading it's listed under.
#[derive(Clone, Debug, PartialEq)]
pub struct Kept {
    pub label: String,
    pub what: String,
    pub group: &'static str,
}

/// The headings, in the order they're listed.
pub const GROUPS: [&str; 5] = ["Launchers and search", "Windows", "Workspaces", "System", "Media"];

// Linux input event codes (as deskkeys has them).
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
const KEY_MICMUTE: u32 = 248;

/// The media keys' names: whatever is held with them, they're named alone.
const MEDIA: [&str; 11] = ["Volume Up", "Volume Down", "Mute", "Play/Pause", "Next", "Previous", "Stop", "Brightness Up", "Brightness Down", "Print", "Mic Mute"];

/// A key (raw code) by name, for the keys the rule covers.
fn raw_name(raw: u32) -> Option<String> {
    Some(match raw {
        KEY_TAB => "Tab".into(),
        KEY_SPACE => "Space".into(),
        KEY_ESC => "Esc".into(),
        KEY_ENTER => "Enter".into(),
        KEY_DELETE => "Delete".into(),
        KEY_HOME => "Home".into(),
        KEY_END => "End".into(),
        KEY_PAGEUP => "PageUp".into(),
        KEY_PAGEDOWN => "PageDown".into(),
        KEY_LEFT => "Left".into(),
        KEY_RIGHT => "Right".into(),
        KEY_UP => "Up".into(),
        KEY_DOWN => "Down".into(),
        KEY_0 => "0".into(),
        r if (KEY_1..KEY_0).contains(&r) => (r - KEY_1 + 1).to_string(),
        r if (KEY_F1..=KEY_F10).contains(&r) => format!("F{}", r - KEY_F1 + 1),
        KEY_F11 => "F11".into(),
        KEY_F12 => "F12".into(),
        KEY_VOLUMEUP => MEDIA[0].into(),
        KEY_VOLUMEDOWN => MEDIA[1].into(),
        KEY_MUTE => MEDIA[2].into(),
        KEY_PLAYPAUSE => MEDIA[3].into(),
        KEY_NEXTSONG => MEDIA[4].into(),
        KEY_PREVIOUSSONG => MEDIA[5].into(),
        KEY_STOPCD => MEDIA[6].into(),
        KEY_BRIGHTNESSUP => MEDIA[7].into(),
        KEY_BRIGHTNESSDOWN => MEDIA[8].into(),
        KEY_SYSRQ => MEDIA[9].into(),
        KEY_MICMUTE => MEDIA[10].into(),
        _ => return None,
    })
}

/// A key and its modifiers by name: "Super+Ctrl+Alt+Shift+Tab" (that order, as
/// deskkeys::UserKey writes them); a media key by its name alone.
fn label(m: Mods, key: &str) -> String {
    if MEDIA.contains(&key) {
        return key.to_string();
    }
    let mods = [(m.logo, "Super"), (m.ctrl, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift")];
    let mut parts: Vec<&str> = mods.iter().filter(|x| x.0).map(|x| x.1).collect();
    parts.push(key);
    parts.join("+")
}

/// A press in a Mac window by name, as kept keys are named: what's compared with the ones taken
/// out.
pub fn press_label(raw: u32, m: Mods) -> Option<String> {
    Some(label(m, &raw_name(raw)?))
}

/// The keys kept for Linux: a superset of typical Linux shortcuts (the standard defaults of
/// Plasma, GNOME, Cinnamon and Omarchy for launchers, switching, workspaces and windows, plus the
/// media keys), each with its heading and a broad description, the same on every desktop. A key
/// this desktop doesn't use still goes on to the Mac (deskkeys.rs). Not Super+Shift or Super+Alt
/// with arrows, which some desktops use too: on the Mac ⌘⇧ and ⌘⌥ arrows select text and switch
/// tabs, so they stay the Mac's.
const SUPERSET: &[(&str, &str, &str)] = &[
    ("Super+Space", GROUPS[0], "Search, menu or keyboard layout"),
    ("Super+Shift+Space", GROUPS[3], "Keyboard layout"),
    ("Alt+Space", GROUPS[0], "Search or window menu"),
    ("Alt+F1", GROUPS[0], "App launcher"),
    ("Alt+F2", GROUPS[0], "Run a command"),
    ("Alt+Shift+F2", GROUPS[0], "Run a command"),
    ("Super+Alt+Space", GROUPS[0], "App launcher"),
    ("Super+Ctrl+Space", GROUPS[0], "Background menu"),
    ("Super+Ctrl+Shift+Space", GROUPS[0], "Theme menu"),
    ("Alt+Tab", GROUPS[1], "Switch windows"),
    ("Alt+Shift+Tab", GROUPS[1], "Switch windows"),
    ("Super+Tab", GROUPS[1], "Switch windows or workspaces"),
    ("Super+Shift+Tab", GROUPS[1], "Switch windows or workspaces"),
    ("Alt+Esc", GROUPS[1], "Switch windows"),
    ("Alt+Shift+Esc", GROUPS[1], "Switch windows"),
    ("Alt+F6", GROUPS[1], "Switch windows of an app"),
    ("Alt+Shift+F6", GROUPS[1], "Switch windows of an app"),
    ("Ctrl+Alt+Tab", GROUPS[1], "Switch panels"),
    ("Ctrl+Alt+Shift+Tab", GROUPS[1], "Switch panels"),
    ("Ctrl+Alt+Esc", GROUPS[1], "Switch panels"),
    ("Ctrl+Alt+Shift+Esc", GROUPS[1], "Switch panels"),
    ("Super+1", GROUPS[2], "Workspace or app 1"),
    ("Super+2", GROUPS[2], "Workspace or app 2"),
    ("Super+3", GROUPS[2], "Workspace or app 3"),
    ("Super+4", GROUPS[2], "Workspace or app 4"),
    ("Super+5", GROUPS[2], "Workspace or app 5"),
    ("Super+6", GROUPS[2], "Workspace or app 6"),
    ("Super+7", GROUPS[2], "Workspace or app 7"),
    ("Super+8", GROUPS[2], "Workspace or app 8"),
    ("Super+9", GROUPS[2], "Workspace or app 9"),
    ("Super+0", GROUPS[2], "Workspace 10 or zoom"),
    ("Super+F1", GROUPS[2], "Workspace 1"),
    ("Super+F2", GROUPS[2], "Workspace 2"),
    ("Super+F3", GROUPS[2], "Workspace 3"),
    ("Super+F4", GROUPS[2], "Workspace 4"),
    ("Super+F7", GROUPS[1], "Show windows"),
    ("Super+F9", GROUPS[1], "Show windows"),
    ("Super+F10", GROUPS[1], "Show windows"),
    ("Super+PageUp", GROUPS[2], "Change workspace or maximise window"),
    ("Super+PageDown", GROUPS[2], "Change workspace or minimise window"),
    ("Super+Home", GROUPS[2], "Change workspace"),
    ("Super+End", GROUPS[2], "Change workspace"),
    ("Super+Ctrl+Left", GROUPS[2], "Change workspace"),
    ("Super+Ctrl+Right", GROUPS[2], "Change workspace"),
    ("Super+Ctrl+Up", GROUPS[2], "Change workspace"),
    ("Super+Ctrl+Down", GROUPS[2], "Change workspace"),
    ("Ctrl+Alt+Left", GROUPS[2], "Change workspace"),
    ("Ctrl+Alt+Right", GROUPS[2], "Change workspace"),
    ("Ctrl+Alt+Up", GROUPS[2], "Change workspace or show all"),
    ("Ctrl+Alt+Down", GROUPS[2], "Change workspace or show all"),
    ("Super+Ctrl+PageUp", GROUPS[1], "Switch tasks"),
    ("Super+Ctrl+PageDown", GROUPS[1], "Switch tasks"),
    ("Super+Shift+1", GROUPS[2], "Move window to workspace 1"),
    ("Super+Shift+2", GROUPS[2], "Move window to workspace 2"),
    ("Super+Shift+3", GROUPS[2], "Move window to workspace 3"),
    ("Super+Shift+4", GROUPS[2], "Move window to workspace 4"),
    ("Super+Shift+5", GROUPS[2], "Move window to workspace 5"),
    ("Super+Shift+6", GROUPS[2], "Move window to workspace 6"),
    ("Super+Shift+7", GROUPS[2], "Move window to workspace 7"),
    ("Super+Shift+8", GROUPS[2], "Move window to workspace 8"),
    ("Super+Shift+9", GROUPS[2], "Move window to workspace 9"),
    ("Super+Shift+0", GROUPS[2], "Move window to workspace 10"),
    ("Super+Shift+PageUp", GROUPS[2], "Move window to another workspace"),
    ("Super+Shift+PageDown", GROUPS[2], "Move window to another workspace"),
    ("Super+Shift+Home", GROUPS[2], "Move window to another workspace"),
    ("Super+Shift+End", GROUPS[2], "Move window to another workspace"),
    ("Super+Ctrl+Shift+Left", GROUPS[2], "Move window to another workspace"),
    ("Super+Ctrl+Shift+Right", GROUPS[2], "Move window to another workspace"),
    ("Super+Ctrl+Shift+Up", GROUPS[2], "Move window to another workspace"),
    ("Super+Ctrl+Shift+Down", GROUPS[2], "Move window to another workspace"),
    ("Ctrl+Alt+Shift+Left", GROUPS[2], "Move window to another workspace"),
    ("Ctrl+Alt+Shift+Right", GROUPS[2], "Move window to another workspace"),
    ("Ctrl+Alt+Shift+Up", GROUPS[2], "Move window to another workspace"),
    ("Ctrl+Alt+Shift+Down", GROUPS[2], "Move window to another workspace"),
    ("Alt+F3", GROUPS[1], "Window menu"),
    ("Alt+F4", GROUPS[1], "Close window"),
    ("Alt+F5", GROUPS[1], "Restore window"),
    ("Alt+F7", GROUPS[1], "Move window"),
    ("Alt+F8", GROUPS[1], "Resize window"),
    ("Alt+F10", GROUPS[1], "Maximise or restore window"),
    ("Super+Ctrl+Esc", GROUPS[1], "Kill a window"),
    ("Super+Ctrl+1", GROUPS[1], "New window of app 1"),
    ("Super+Ctrl+2", GROUPS[1], "New window of app 2"),
    ("Super+Ctrl+3", GROUPS[1], "New window of app 3"),
    ("Super+Ctrl+4", GROUPS[1], "New window of app 4"),
    ("Super+Ctrl+5", GROUPS[1], "New window of app 5"),
    ("Super+Ctrl+6", GROUPS[1], "New window of app 6"),
    ("Super+Ctrl+7", GROUPS[1], "New window of app 7"),
    ("Super+Ctrl+8", GROUPS[1], "New window of app 8"),
    ("Super+Ctrl+9", GROUPS[1], "New window of app 9"),
    ("Ctrl+Alt+Delete", GROUPS[3], "Log out"),
    ("Volume Up", GROUPS[4], "Volume up"),
    ("Volume Down", GROUPS[4], "Volume down"),
    ("Mute", GROUPS[4], "Mute"),
    ("Mic Mute", GROUPS[4], "Mic mute"),
    ("Play/Pause", GROUPS[4], "Play or pause"),
    ("Next", GROUPS[4], "Next track"),
    ("Previous", GROUPS[4], "Previous track"),
    ("Stop", GROUPS[4], "Stop playing"),
    ("Brightness Up", GROUPS[3], "Brightness up"),
    ("Brightness Down", GROUPS[3], "Brightness down"),
    ("Print", GROUPS[3], "Screenshot"),
];

/// The keys kept for Linux, grouped (`GROUPS` order, each heading's keys in SUPERSET's order).
pub fn all() -> Vec<Kept> {
    let mut keys: Vec<Kept> = SUPERSET.iter().map(|(label, group, what)| Kept { label: (*label).into(), what: (*what).into(), group }).collect();
    keys.sort_by_key(|k| GROUPS.iter().position(|g| *g == k.group).unwrap_or(GROUPS.len()));
    keys
}

/// A press is one of the keys kept for Linux (whether or not the person took it out).
pub fn is_kept(raw: u32, m: Mods) -> bool {
    let Some(label) = press_label(raw, m) else { return false };
    SUPERSET.iter().any(|(l, ..)| *l == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_key_can_be_pressed_and_named_the_same() {
        let sup = Mods { logo: true, ..Default::default() };
        assert_eq!(press_label(KEY_TAB, sup).as_deref(), Some("Super+Tab"));
        assert_eq!(press_label(KEY_1, Mods { logo: true, shift: true, ..Default::default() }).as_deref(), Some("Super+Shift+1"));
        assert_eq!(press_label(KEY_VOLUMEUP, sup).as_deref(), Some("Volume Up"));
        // The same list everywhere, every key under one of the headings, with a description.
        let all = all();
        assert_eq!(all.len(), SUPERSET.len());
        assert!(all.iter().all(|k| GROUPS.contains(&k.group) && !k.what.is_empty()));
        assert!(all.iter().any(|k| k.label == "Alt+F7") && all.iter().any(|k| k.label == "Super+Shift+1"));
        // Opt+digit types a character on the Mac: never kept.
        assert!(!SUPERSET.iter().any(|k| k.0 == "Alt+1"));
        // Every listed key's name is one a press can have (no typos that would never match).
        let names: Vec<String> = (1..=255u32)
            .flat_map(|raw| {
                let mut out = Vec::new();
                for bits in 0..16u8 {
                    let m = Mods { logo: bits & 1 != 0, ctrl: bits & 2 != 0, alt: bits & 4 != 0, shift: bits & 8 != 0 };
                    out.extend(press_label(raw, m));
                }
                out
            })
            .collect();
        for (label, ..) in SUPERSET {
            assert!(names.iter().any(|n| n == label), "{label} can't be pressed");
        }
    }
}
