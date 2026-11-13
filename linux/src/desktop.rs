//! Sweeflow's KWin rules (kwinrulesrc), written and removed without touching anyone else's.
//! Today that is only taking out the rules earlier versions wrote: Mac windows on a desktop of
//! their own, and Mac windows kept out of the task manager. Both features are gone.

use std::process::Command;

use anyhow::Result;

/// The rule that put Mac windows on a virtual desktop of their own.
const OWN_DESKTOP_RULE: &str = "sweeflow";

/// Takes the own-desktop rule out, where a version that had the feature left it. (The "Mac"
/// desktop it made stays, like any desktop: it may have windows on it.)
pub fn retire_own_desktop() {
    if has_rule(OWN_DESKTOP_RULE) {
        match set_rule(OWN_DESKTOP_RULE, None) {
            Ok(()) => log::info!("removed the old rule that put Mac windows on their own desktop"),
            Err(e) => log::warn!("removing the own-desktop rule: {e:#}"),
        }
    }
}

/// Writes (Some) or removes (None) one of our KWin rules, keeping everyone else's, and has
/// KWin reload them.
pub fn set_rule(name: &str, entries: Option<&[(&str, &str)]>) -> Result<()> {
    let set = |group: &str, key: &str, value: &str| {
        Command::new("kwriteconfig6").args(["--file", "kwinrulesrc", "--group", group, "--key", key, value]).status()
    };
    let mut rules = other_rules(&rule_list()?, name);
    if let Some(entries) = entries {
        for (k, v) in entries {
            set(name, k, v)?;
        }
        rules.push(name.into());
    } else {
        // kwriteconfig6 can't drop a whole group: delete the keys we write.
        for k in ["Description", "wmclass", "wmclassmatch", "wmclasscomplete", "desktops", "desktopsrule", "skiptaskbar", "skiptaskbarrule"] {
            Command::new("kwriteconfig6").args(["--file", "kwinrulesrc", "--group", name, "--key", k, "--delete"]).status()?;
        }
    }
    // Both keys: KWin 6.8 keeps the list as Order (moving "rules" there when it reloads), older
    // ones as rules.
    set("General", "Order", &rules.join(","))?;
    set("General", "rules", &rules.join(","))?;
    set("General", "count", &rules.len().to_string())?;
    Command::new("qdbus6").args(["org.kde.KWin", "/KWin", "reconfigure"]).status()?;
    Ok(())
}

/// Whether one of our KWin rules is in place.
pub fn has_rule(name: &str) -> bool {
    rule_list().is_ok_and(|list| list.trim().split(',').any(|r| r == name))
}

/// kwinrulesrc's list of rules (comma-separated): Order on KWin 6.8, which empties "rules" when
/// it reloads; rules before that.
fn rule_list() -> Result<String> {
    let read = |key: &str| -> Result<String> {
        let o = Command::new("kreadconfig6").args(["--file", "kwinrulesrc", "--group", "General", "--key", key]).output()?;
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let order = read("Order")?;
    Ok(if order.is_empty() { read("rules")? } else { order })
}

/// kwinrulesrc's rule list without `name` (it's added back when switched on).
fn other_rules(list: &str, name: &str) -> Vec<String> {
    list.trim().split(',').filter(|r| !r.is_empty() && *r != name).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_peoples_rules_are_kept() {
        assert_eq!(other_rules("abc,sweeflow,def\n", OWN_DESKTOP_RULE), ["abc", "def"]);
        assert_eq!(other_rules("", OWN_DESKTOP_RULE), Vec::<String>::new());
        assert_eq!(other_rules("sweeflow", OWN_DESKTOP_RULE), Vec::<String>::new());
        assert_eq!(other_rules("sweeflow,sweeflow-taskbar", "sweeflow-taskbar"), ["sweeflow"]);
    }
}

/// Puts a window that floats (a fixed-size Mac window) at the same spot of the screen as its app
/// put it on the Mac: `spot` is fractions of the room around it (0 left/top, 1 right/bottom).
/// Wayland lets no app place its own windows, so the desktop does it: a one-off KWin script on
/// Plasma, Hyprland's own move there. GNOME has no way (it places dialogs itself). A moment
/// after the window opens, once the desktop has it.
pub fn place_at_spot(class: String, title: String, spot: (f32, f32)) {
    let (fx, fy) = (spot.0.clamp(0.0, 1.0), spot.1.clamp(0.0, 1.0));
    let kde = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').any(|d| d.eq_ignore_ascii_case("KDE"));
    if !kde && !crate::hyprland::running() {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if crate::hyprland::running() {
            crate::hyprland::place_window(&class, &title, (fx, fy));
            return;
        }
        let js = format!(
            "for (const w of workspace.windowList()) {{ if (w.resourceClass == {c} && w.caption == {t}) {{ \
               const a = workspace.clientArea(KWin.PlacementArea, w); const g = w.frameGeometry; \
               w.frameGeometry = {{ x: Math.round(a.x + {fx} * (a.width - g.width)), y: Math.round(a.y + {fy} * (a.height - g.height)), width: g.width, height: g.height }}; }} }}",
            c = serde_json::to_string(&class).unwrap_or_default(),
            t = serde_json::to_string(&title).unwrap_or_default(),
        );
        // A file of its own each time: KWin keeps a loaded script's code by its path.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let (path, name) = (dir.join(format!("sweeflow-place-{}-{n}.js", std::process::id())), format!("sweeflow-place-{n}"));
        if std::fs::write(&path, js).is_err() {
            return;
        }
        let Ok(bus) = zbus::blocking::Connection::session() else { return };
        let id: Option<i32> = bus
            .call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript", &(path.to_string_lossy().as_ref(), name.as_str()))
            .ok()
            .and_then(|r| r.body().deserialize().ok());
        if let Some(id) = id {
            let _ = bus.call_method(Some("org.kde.KWin"), format!("/Scripting/Script{id}").as_str(), Some("org.kde.kwin.Script"), "run", &());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = bus.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(name.as_str(),));
        let _ = std::fs::remove_file(&path);
    });
}
