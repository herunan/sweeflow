//! Sweeflow's keys from any window, asked of the desktop the standard way: the Global Shortcuts
//! portal (org.freedesktop.portal.GlobalShortcuts). The running client holds the session (the
//! keys last as long as it does) and gets the presses. Used on Plasma and on GNOME 48 and later;
//! the first time, the desktop shows its own dialog to confirm the keys (preferred: the ones set
//! in Sweeflow's Shortcuts window, keys.rs). Changing them later is the desktop's dialog again
//! (ConfigureShortcuts), opened from Sweeflow's Shortcuts window; what it gives back is saved as
//! Sweeflow's keys, so Mac windows (ui.rs) follow. Hyprland keeps its own bindings (hyprland.rs:
//! its portal assigns no key without one anyway), and desktops without the portal (older GNOME,
//! Cinnamon) the shortcuts `sweeflow install` writes into their settings (setup.rs).

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

use anyhow::{bail, ensure, Context, Result};
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
/// What the desktop lists them under: Sweeflow's launcher (sweeflow.desktop).
const APP_ID: &str = "sweeflow";
/// The keys from any window: the Global menu acts only in Mac windows, so it isn't one.
const SHORTCUTS: [(&str, &str); 3] = [("open", "Open Sweeflow"), ("pause", "Pause or resume streaming"), ("stop", "Stop or start streaming")];

/// The session, once bound: for opening the desktop's dialog to change the keys.
static SESSION: Mutex<Option<(Connection, OwnedObjectPath)>> = Mutex::new(None);

/// The desktop has the portal for this (and isn't Hyprland, which keeps its bindings).
pub fn available() -> bool {
    !crate::hyprland::running()
        && Connection::session().is_ok_and(|bus| {
            // Asked directly: a proxy fetches every property first and, without the interface,
            // printed zbus's warning into `sweeflow install`'s output.
            bus.call_method(Some(DEST), PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(IFACE, "version")).is_ok()
        })
}

/// The keys go through the portal on this desktop (bound at least once).
pub fn in_use() -> bool {
    crate::config::dir().join(NOTE).exists()
}

/// Says the keys went through the portal: from then on `sweeflow install` leaves the desktop's
/// own settings alone, and the old shortcuts it wrote there are gone.
const NOTE: &str = "keys.portal";

/// Asks for the keys and waits for their presses, on a thread of its own. Without the portal,
/// nothing: the desktop's settings have them (setup.rs).
pub fn start() {
    std::thread::spawn(|| match run() {
        Ok(()) => {}
        Err(e) => log::info!("global shortcuts portal: {e:#}"),
    });
}

fn run() -> Result<()> {
    if !available() {
        bail!("not on this desktop: its own shortcut settings have Sweeflow's keys");
    }
    let bus = Connection::session()?;
    // Host apps say who they are (portal 1.19+); GNOME refuses one without a known launcher.
    let no_options: HashMap<&str, Value> = HashMap::new();
    if let Err(e) = bus.call_method(Some(DEST), PATH, Some("org.freedesktop.host.portal.Registry"), "Register", &(APP_ID, &no_options)) {
        log::debug!("portal registry: {e}");
    }
    // The shortcuts Sweeflow wrote into the desktop's settings before: gone first, or they'd
    // keep the keys (silently on GNOME, with a warning in Plasma's dialog).
    if !in_use() {
        crate::setup::drop_desktop_keys();
    }
    let sender = bus.unique_name().context("no name on the bus")?.trim_start_matches(':').replace('.', "_");
    let created = request(&bus, &sender, "create", |token| {
        let opts: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token)), ("session_handle_token", Value::from("sweeflow"))]);
        bus.call_method(Some(DEST), PATH, Some(IFACE), "CreateSession", &(opts,))
    })?;
    let session = OwnedObjectPath::try_from(
        String::try_from(created.get("session_handle").context("no session")?.clone()).context("session handle")?,
    )?;

    let remapped = crate::setup::mac_keys_remapped();
    let shortcuts: Vec<(&str, HashMap<&str, Value>)> = SHORTCUTS
        .iter()
        .map(|(id, description)| {
            let mut info = HashMap::from([("description", Value::from(*description))]);
            if let Some(k) = crate::keys::own_key(id) {
                info.insert("preferred_trigger", Value::from(trigger(&k, remapped)));
            }
            (*id, info)
        })
        .collect();
    let bound = request(&bus, &sender, "bind", |token| {
        let opts: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token))]);
        bus.call_method(Some(DEST), PATH, Some(IFACE), "BindShortcuts", &(&session, &shortcuts, "", opts))
    })?;
    let _ = std::fs::write(crate::config::dir().join(NOTE), "");
    if let Some(list) = bound.get("shortcuts") {
        save_triggers(list, remapped);
        if crate::keys::desktop().contains("KDE") {
            plasma_fill_in(list, remapped);
        }
    }
    log::info!("global shortcuts portal: Sweeflow's keys are the desktop's");
    if crate::keys::desktop().contains("GNOME") {
        watch_gnome(remapped);
    }
    *SESSION.lock().unwrap() = Some((bus.clone(), session.clone()));

    let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE)?.build();
    for msg in MessageIterator::for_match_rule(rule, &bus, None)? {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        match header.member().map(|m| m.as_str()) {
            Some("Activated") => {
                let Ok((from, id, _when, _opts)) = msg.body().deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>() else { continue };
                if from != session {
                    continue;
                }
                act(&id);
            }
            Some("ShortcutsChanged") => {
                let Ok((from, list)) = msg.body().deserialize::<(OwnedObjectPath, OwnedValue)>() else { continue };
                if from == session {
                    save_triggers(&list, remapped);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The desktop's dialog for changing Sweeflow's keys (ConfigureShortcuts, portal version 2).
/// False: no session (not on this desktop), or the desktop can't.
pub fn configure() -> bool {
    let Some((bus, session)) = SESSION.lock().unwrap().clone() else { return false };
    let opts: HashMap<&str, Value> = HashMap::new();
    match bus.call_method(Some(DEST), PATH, Some(IFACE), "ConfigureShortcuts", &(&session, "", opts)) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("the desktop's dialog for Sweeflow's keys: {e}");
            false
        }
    }
}

/// Plasma: a shortcut the portal bound without a key (its dialog closed without one, or the key
/// was taken) gets Sweeflow's key as set, in Plasma's own list of shortcuts, where the portal
/// session finds it.
fn plasma_fill_in(list: &OwnedValue, remapped: bool) {
    let Ok(list) = Vec::<(String, HashMap<String, OwnedValue>)>::try_from(list.clone()) else { return };
    let Ok(bus) = Connection::session() else { return };
    for (id, info) in list {
        let given = info.get("trigger_description").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        if !given.trim().is_empty() {
            continue;
        }
        let Some(k) = crate::keys::own_key(&id) else { continue };
        let codes: Vec<i32> = as_linux_gets(&k, remapped).iter().filter_map(|k| k.qt()).collect();
        if codes.is_empty() {
            continue;
        }
        let name = SHORTCUTS.iter().find(|(i, _)| *i == id).map_or(id.as_str(), |(_, n)| n);
        // A list of strings (as), not four separate ones: KGlobalAccel takes (as, ai).
        let action = vec![APP_ID, id.as_str(), "Sweeflow", name];
        match bus.call_method(Some("org.kde.kglobalaccel"), "/kglobalaccel", Some("org.kde.KGlobalAccel"), "setForeignShortcut", &(action, codes)) {
            Ok(_) => log::info!("Plasma: {name} on {}", k.label()),
            Err(e) => log::warn!("Plasma: couldn't give {name} its key: {e}"),
        }
    }
}

/// The key as Linux apps get it when it's pressed. Behind a Mac-style remapper (Toshy, Kinto)
/// that isn't the key pressed, and it depends on the keyboard (Toshy's modmaps): on an Apple one
/// ⌘ (Super) becomes Ctrl and Ctrl Super; on a PC one Alt (in ⌘'s place) becomes Ctrl, Super
/// (in ⌥'s) Alt, and Ctrl Super. Which keyboard isn't known, so both: Plasma takes two keys
/// for a shortcut. Super+Alt comes out as Ctrl+Alt on either.
pub fn as_linux_gets(k: &crate::deskkeys::UserKey, remapped: bool) -> Vec<crate::deskkeys::UserKey> {
    if !remapped {
        return vec![k.clone()];
    }
    let apple = crate::deskkeys::UserKey { logo: k.ctrl, ctrl: k.logo, ..k.clone() };
    let pc = crate::deskkeys::UserKey { logo: k.ctrl, alt: k.logo, ctrl: k.alt, ..k.clone() };
    if apple == pc { vec![apple] } else { vec![apple, pc] }
}

/// Plasma: one of Sweeflow's keys changed in its Shortcuts window (keys.json) is given to the
/// portal's shortcut in Plasma's own list. Plasma's portal has no dialog for changing them
/// (ConfigureShortcuts does nothing there), so Sweeflow sets it, as for a key the portal bound
/// without one.
pub fn plasma_set(id: &str) {
    let Some((_, name)) = SHORTCUTS.iter().find(|(i, _)| *i == id) else { return };
    let Ok(bus) = Connection::session() else { return };
    let remapped = crate::setup::mac_keys_remapped();
    let key = crate::keys::own_key(id);
    let codes: Vec<i32> = key.as_ref().map(|k| as_linux_gets(k, remapped)).unwrap_or_default().iter().filter_map(|k| k.qt()).collect();
    // A list of strings (as), not four separate ones: KGlobalAccel takes (as, ai).
    let action = vec![APP_ID, id, "Sweeflow", name];
    match bus.call_method(Some("org.kde.kglobalaccel"), "/kglobalaccel", Some("org.kde.KGlobalAccel"), "setForeignShortcut", &(action, codes)) {
        Ok(_) => log::info!(
            "Plasma: {name} on {}",
            key.map_or("nothing".to_string(), |k| as_linux_gets(&k, remapped).iter().map(|k| k.label()).collect::<Vec<_>>().join(" or "))
        ),
        Err(e) => log::warn!("Plasma: couldn't give {name} its key: {e}"),
    }
}

/// GNOME keeps the portal's keys in its own settings (this list, which GNOME Settings changes
/// too) and applies a change at once; its portal has no dialog for them (GNOME 50) or refuses
/// Sweeflow's (CachyOS). So Sweeflow writes them there, and watches them (watch_gnome).
const GNOME_KEYS: &str = "/org/gnome/settings-daemon/global-shortcuts/sweeflow/shortcuts";

/// GNOME: Sweeflow's keys as set (keys.json) written into GNOME's list of them.
pub fn gnome_set() {
    let remapped = crate::setup::mac_keys_remapped();
    let items: Vec<String> = SHORTCUTS
        .iter()
        .map(|(id, description)| {
            let accel = crate::keys::own_key(id).map(|k| crate::setup::gtk_accel(&k, remapped)).unwrap_or_default();
            let keys = if accel.is_empty() { "@as []".to_string() } else { format!("['{accel}']") };
            format!("('{id}', {{'shortcuts': <{keys}>, 'description': <'{description}'>}})")
        })
        .collect();
    let value = format!("[{}]", items.join(", "));
    match Command::new("dconf").args(["write", GNOME_KEYS, &value]).status() {
        Ok(s) if s.success() => log::info!("GNOME: Sweeflow's keys set ({value})"),
        Ok(s) => log::warn!("GNOME: couldn't set Sweeflow's keys (dconf {s})"),
        Err(e) => log::warn!("GNOME: couldn't set Sweeflow's keys: {e}"),
    }
}

/// GNOME: a key changed in GNOME Settings is saved as Sweeflow's (keys.json), so its windows and
/// Mac windows follow. GNOME's portal doesn't say (no ShortcutsChanged), so its list is watched.
fn watch_gnome(remapped: bool) {
    std::thread::spawn(move || {
        let read = || Command::new("dconf").args(["read", GNOME_KEYS]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let apply = |text: &str| {
            for (id, _) in SHORTCUTS {
                let Some(after) = text.split(&format!("('{id}', {{'shortcuts': <")).nth(1) else { continue };
                // ['<Alt><Super>y'] or @as [] (none).
                let first = after.split(']').next().unwrap_or_default().split('\'').nth(1).unwrap_or_default();
                let now = if first.is_empty() { None } else { from_description(first, remapped) };
                let had = crate::keys::own_key(id);
                if now.as_ref().map(|k| k.label()) != had.as_ref().map(|k| k.label()) {
                    log::info!("GNOME: {id} is {} now (GNOME Settings)", now.as_ref().map_or("nothing".to_string(), |k| k.label()));
                    let _ = crate::keys::set_own_key(id, &now.map(|k| k.label()).unwrap_or_default());
                }
            }
        };
        let Ok(mut child) = Command::new("dconf").args(["watch", "/org/gnome/settings-daemon/global-shortcuts/sweeflow/"]).stdout(std::process::Stdio::piped()).spawn() else { return };
        let Some(out) = child.stdout.take() else { return };
        use std::io::BufRead;
        for line in std::io::BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if line.trim().ends_with("/shortcuts") {
                apply(&read());
            }
        }
    });
}

/// A key pressed: what it does.
fn act(id: &str) {
    match id {
        "open" => crate::ui::run_own("open", "Sweeflow"),
        "pause" => crate::tray::panel_do("pause-toggle", ""),
        "stop" => crate::tray::panel_do("stream-toggle", ""),
        other => log::debug!("global shortcut {other}: not Sweeflow's"),
    }
}

/// A portal request: called with its token, then its Response waited for (as long as the
/// desktop's dialog is up). The results, or why not.
fn request(bus: &Connection, sender: &str, token: &str, call: impl FnOnce(&str) -> zbus::Result<zbus::Message>) -> Result<HashMap<String, OwnedValue>> {
    let path = format!("{PATH}/request/{sender}/{token}");
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .path(path.as_str())?
        .build();
    let mut responses = MessageIterator::for_match_rule(rule, bus, Some(1))?;
    call(token)?;
    let msg = responses.next().context("no answer")??;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    ensure!(code == 0, "{token}: {}", if code == 1 { "cancelled" } else { "refused" });
    Ok(results)
}

/// A key as the portal writes a trigger (the XDG shortcuts spec): "LOGO+ALT+o". Behind a
/// Mac-style remapper, ⌘ reaches Linux apps as Ctrl, so that's the key asked for.
fn trigger(k: &crate::deskkeys::UserKey, remapped: bool) -> String {
    let mut parts = Vec::new();
    if k.ctrl || (k.logo && remapped) {
        parts.push("CTRL".to_string());
    }
    if k.alt {
        parts.push("ALT".into());
    }
    if k.shift {
        parts.push("SHIFT".into());
    }
    if k.logo && !remapped {
        parts.push("LOGO".into());
    }
    parts.push(if k.key.chars().count() == 1 { k.key.to_lowercase() } else { k.key.clone() });
    parts.join("+")
}

/// The keys the desktop settled on (its dialog), saved as Sweeflow's: Mac windows and its
/// windows follow. Back in Mac terms behind a remapper (Ctrl there is ⌘).
fn save_triggers(list: &OwnedValue, remapped: bool) {
    let Ok(list) = Vec::<(String, HashMap<String, OwnedValue>)>::try_from(list.clone()) else { return };
    for (id, info) in list {
        let Some(text) = info.get("trigger_description").and_then(|v| String::try_from(v.clone()).ok()) else { continue };
        match from_description(&text, remapped) {
            Some(k) => {
                let _ = crate::keys::set_own_key(&id, &k.label());
            }
            // No key given back (Plasma leaves it out when it hasn't assigned one yet): kept as set.
            None if text.trim().is_empty() => {}
            None => log::debug!("{id}: the desktop's key “{text}” isn't one Sweeflow reads"),
        }
    }
}

/// The desktop's words for a key: GNOME's "Press <Alt><Super>o", Plasma's "Meta+Alt+O".
fn from_description(text: &str, remapped: bool) -> Option<crate::deskkeys::UserKey> {
    let text = text.trim().trim_start_matches("Press ").trim();
    if text.is_empty() {
        return None;
    }
    // GNOME's <Mod> form, in the shortcuts spec's words.
    let plus = if text.starts_with('<') {
        let mut mods = Vec::new();
        let mut rest = text;
        while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
            mods.push(&rest[1..=end]);
            rest = &rest[end + 2..];
        }
        mods.push(rest);
        mods.join("+")
    } else {
        text.to_string()
    };
    let mut k = crate::deskkeys::UserKey::parse(&plus.replace("Primary", "Ctrl").replace("Control", "Ctrl"))?;
    if remapped && k.ctrl && !k.logo {
        k.ctrl = false;
        k.logo = true;
    }
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deskkeys::UserKey;

    #[test]
    fn triggers_both_ways() {
        let k = UserKey::parse("Super+Alt+O").unwrap();
        assert_eq!(trigger(&k, false), "ALT+LOGO+o");
        assert_eq!(trigger(&k, true), "CTRL+ALT+o");
        assert_eq!(from_description("Press <Alt><Super>o", false).unwrap().label(), "Super+Alt+O");
        assert_eq!(from_description("Meta+Alt+O", false).unwrap().label(), "Super+Alt+O");
        assert_eq!(from_description("Ctrl+Alt+Y", true).unwrap().label(), "Super+Alt+Y");
        assert!(from_description("", false).is_none());
    }
}

#[cfg(test)]
mod remapper_tests {
    use crate::deskkeys::UserKey;

    #[test]
    fn behind_a_remapper_both_keyboards_forms() {
        let k = |s: &str| UserKey::parse(s).unwrap();
        let labels = |s: &str, r: bool| super::as_linux_gets(&k(s), r).iter().map(|k| k.label()).collect::<Vec<_>>();
        assert_eq!(labels("Super+Shift+,", false), ["Super+Shift+,"]);
        // Apple keyboard: ⌘ is Ctrl; PC keyboard: Super (in ⌥'s place) is Alt.
        assert_eq!(labels("Super+Shift+,", true), ["Ctrl+Shift+,", "Alt+Shift+,"]);
        // The defaults: one form.
        assert_eq!(labels("Super+Alt+Y", true), ["Ctrl+Alt+Y"]);
    }
}
