//! Mac windows on a virtual desktop of their own ("Mac"), apart from the Linux ones: a KWin
//! rule places each new Mac window there (initially, so one can still be moved elsewhere). An
//! existing empty desktop is reused rather than adding one. Switched off, the rule goes, and
//! the desktop goes too (or, if it was the user's, gets its old name back).

use std::{
    fs,
    process::Command,
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use zbus::blocking::Connection;
use zvariant::OwnedValue;

const NAME: &str = "Mac";
const RULE: &str = "sweeflow";

/// Applies the setting; safe to repeat. Runs D-Bus calls and a KWin script: call off the UI.
pub fn apply(on: bool) {
    std::thread::spawn(move || {
        let r = if on { enable() } else { disable() };
        if let Err(e) = r {
            log::warn!("Mac desktop: {e:#}");
        }
    });
}

fn desktops(conn: &Connection) -> Result<Vec<(u32, String, String)>> {
    let reply = conn.call_method(
        Some("org.kde.KWin"),
        "/VirtualDesktopManager",
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.kde.KWin.VirtualDesktopManager", "desktops"),
    )?;
    let v: OwnedValue = reply.body().deserialize()?;
    Ok(Vec::<(u32, String, String)>::try_from(v)?)
}

/// The id of the desktop in use, kept so we only ever remove that one. A second line, when
/// present, is the name an empty desktop of the user's had before we took it over: it gets that
/// name back (instead of being removed) when switched off.
fn id_file() -> std::path::PathBuf {
    crate::config::dir().join("mac-desktop")
}

fn stored() -> (String, Option<String>) {
    let text = fs::read_to_string(id_file()).unwrap_or_default();
    let mut lines = text.lines();
    let id = lines.next().unwrap_or_default().trim().to_string();
    (id, lines.next().map(|n| n.to_string()).filter(|n| !n.is_empty()))
}

/// The reply from the look-up script (see `find`), via panel.rs.
static FOUND: Mutex<Option<(String, String)>> = Mutex::new(None);
static FOUND_CV: Condvar = Condvar::new();

pub fn found(id: String, adopted_name: String) {
    *FOUND.lock().unwrap() = Some((id, adopted_name));
    FOUND_CV.notify_all();
}

/// Asks KWin for the desktop to use: ours if it's still there, else one called Mac, else the
/// last desktop with no windows on it (the user's spare). None: make one.
fn find(ours: &str) -> Result<Option<(String, Option<String>)>> {
    *FOUND.lock().unwrap() = None;
    let js = format!(
        "let d = workspace.desktops.find(d => d.id == '{ours}') || workspace.desktops.find(d => d.name == '{NAME}');\n\
         let adopted = '';\n\
         if (!d) {{\n\
           const used = new Set();\n\
           for (const w of workspace.windowList())\n\
             if (w.normalWindow && !w.skipTaskbar && !w.onAllDesktops) for (const x of w.desktops) used.add(x.id);\n\
           const empty = workspace.desktops.filter(x => !used.has(x.id));\n\
           if (empty.length) {{ d = empty[empty.length - 1]; adopted = d.name; }}\n\
         }}\n\
         callDBus('dev.sweeflow.Client', '/dev/sweeflow/Panel', 'dev.sweeflow.Panel', 'DesktopFound', d ? d.id : '', adopted);"
    );
    run_script(&js)?;
    let guard = FOUND.lock().unwrap();
    let (guard, _) = FOUND_CV.wait_timeout_while(guard, Duration::from_secs(3), |f| f.is_none()).unwrap();
    let (id, adopted) = guard.clone().context("KWin didn't answer the desktop look-up")?;
    Ok((!id.is_empty()).then(|| (id, (!adopted.is_empty()).then_some(adopted))))
}

fn enable() -> Result<()> {
    let conn = Connection::session()?;
    let (ours, adopted_before) = stored();
    let id = match find(&ours)? {
        Some((id, _)) if id == ours => id,
        Some((id, adopted)) => {
            // The user's own: called Mac already, or an empty one taken over and renamed.
            let adopted = adopted.filter(|n| n != NAME);
            if adopted.is_some() {
                conn.call_method(
                    Some("org.kde.KWin"),
                    "/VirtualDesktopManager",
                    Some("org.kde.KWin.VirtualDesktopManager"),
                    "setDesktopName",
                    &(id.as_str(), NAME),
                )?;
            }
            log::info!("using the existing desktop {} for Mac apps", adopted.as_deref().unwrap_or(NAME));
            save(&id, adopted.as_deref().or(adopted_before.as_deref().filter(|_| id == ours)))?;
            id
        }
        None => {
            let list = desktops(&conn)?;
            conn.call_method(
                Some("org.kde.KWin"),
                "/VirtualDesktopManager",
                Some("org.kde.KWin.VirtualDesktopManager"),
                "createDesktop",
                &(list.len() as u32, NAME),
            )?;
            let list = desktops(&conn)?;
            let (_, id, _) = list.iter().rev().find(|(_, _, n)| n == NAME).context("desktop not created")?;
            save(id, None)?;
            log::info!("created the {NAME} desktop");
            id.clone()
        }
    };
    write_rule(Some(&id))?;
    // Windows already open move there too: now, and again shortly for any that were opening
    // while the rule went in (right after connecting, they all are).
    let js = format!(
        "const d = workspace.desktops.find(d => d.id == '{id}');\n\
         if (d) for (const w of workspace.windowList()) if (w.resourceClass.startsWith('sweeflow-') && w.resourceClass != 'sweeflow-display') w.desktops = [d];"
    );
    run_script(&js)?;
    std::thread::sleep(Duration::from_secs(3));
    run_script(&js)
}

fn save(id: &str, adopted: Option<&str>) -> Result<()> {
    fs::create_dir_all(crate::config::dir())?;
    fs::write(id_file(), format!("{id}\n{}", adopted.unwrap_or_default()))?;
    Ok(())
}

/// "Gather" in the tray: set the Mac desktop up again and bring every Mac window back to it,
/// whatever happened to it meanwhile.
pub fn gather() {
    if !crate::tray::wants_own_desktop() {
        return;
    }
    std::thread::spawn(|| {
        log::info!("gathering Mac windows onto the Mac desktop");
        if let Err(e) = enable() {
            log::warn!("Mac desktop: {e:#}");
        }
    });
}

/// A Mac window is opening: make sure the desktop the rule sends it to still exists (it may
/// have been removed by hand), at most every few seconds.
pub fn check() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) || !crate::tray::wants_own_desktop() {
            return;
        }
        *last = Some(Instant::now());
    }
    std::thread::spawn(|| {
        let (ours, _) = stored();
        let gone = Connection::session()
            .and_then(|c| Ok(desktops(&c).map(|l| !l.iter().any(|(_, id, _)| *id == ours)).unwrap_or(false)))
            .unwrap_or(false);
        if gone {
            log::info!("the Mac desktop is gone: setting it up again");
            if let Err(e) = enable() {
                log::warn!("Mac desktop: {e:#}");
            }
        }
    });
}

fn disable() -> Result<()> {
    let had_rule = has_rule(RULE);
    write_rule(None)?;
    let (id, adopted) = stored();
    if !had_rule && id.is_empty() {
        return Ok(());
    }
    if !id.is_empty() {
        remove_or_rename(&id, adopted.as_deref())?;
    }
    let _ = fs::remove_file(id_file());
    // KWin reloads rules a little after being asked, so a Mac window opening meanwhile still
    // gets the old rule, naming a desktop that's gone by then: KWin puts it on all desktops
    // (and the tiler leaves it floating). Bring any such window to the desktop in use: now, and
    // again shortly, once the reload has surely happened.
    let js = "for (const w of workspace.windowList())\n\
              if (w.onAllDesktops && w.resourceClass.startsWith('sweeflow-') && w.resourceClass != 'sweeflow-display') w.desktops = [workspace.currentDesktop];";
    run_script(js)?;
    std::thread::sleep(Duration::from_secs(3));
    run_script(js)
}

fn remove_or_rename(id: &str, adopted: Option<&str>) -> Result<()> {
    let conn = Connection::session()?;
    if desktops(&conn)?.iter().any(|(_, d, _)| *d == id) {
        if let Some(name) = adopted {
            // The user's own desktop: give it its name back, windows and all.
            conn.call_method(
                Some("org.kde.KWin"),
                "/VirtualDesktopManager",
                Some("org.kde.KWin.VirtualDesktopManager"),
                "setDesktopName",
                &(id, name),
            )?;
            log::info!("gave the desktop its name {name} back");
        } else {
            // Any windows on it move to a neighbouring desktop.
            conn.call_method(
                Some("org.kde.KWin"),
                "/VirtualDesktopManager",
                Some("org.kde.KWin.VirtualDesktopManager"),
                "removeDesktop",
                &(id,),
            )?;
            log::info!("removed the {NAME} desktop");
        }
    }
    Ok(())
}

/// Adds (with a desktop id) or removes our rule in kwinrulesrc, keeping everyone else's.
fn write_rule(desktop: Option<&str>) -> Result<()> {
    match desktop {
        Some(id) => set_rule(
            RULE,
            Some(&[
                ("Description", "Mac apps on their own desktop (sweeflow)"),
                // Not the Mac display view: it opens wherever you are.
                ("wmclass", "^sweeflow-(?!display$)"),
                ("wmclassmatch", "3"),
                ("wmclasscomplete", "false"),
                ("desktops", id),
                // Apply initially: placed there when opened, free to move afterwards.
                ("desktopsrule", "3"),
            ]),
        ),
        None => set_rule(RULE, None),
    }
}

/// Writes (Some) or removes (None) one of our KWin rules, keeping everyone else's, and has
/// KWin reload them.
pub fn set_rule(name: &str, entries: Option<&[(&str, &str)]>) -> Result<()> {
    let set = |group: &str, key: &str, value: &str| {
        Command::new("kwriteconfig6").args(["--file", "kwinrulesrc", "--group", group, "--key", key, value]).status()
    };
    let read = Command::new("kreadconfig6").args(["--file", "kwinrulesrc", "--group", "General", "--key", "rules"]).output()?;
    let mut rules = other_rules(&String::from_utf8_lossy(&read.stdout), name);
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
    set("General", "rules", &rules.join(","))?;
    set("General", "count", &rules.len().to_string())?;
    Command::new("qdbus6").args(["org.kde.KWin", "/KWin", "reconfigure"]).status()?;
    Ok(())
}

/// Whether one of our KWin rules is in place.
pub fn has_rule(name: &str) -> bool {
    Command::new("kreadconfig6")
        .args(["--file", "kwinrulesrc", "--group", "General", "--key", "rules"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().split(',').any(|r| r == name))
}

/// kwinrulesrc's rule list without `name` (it's added back when switched on).
fn other_rules(list: &str, name: &str) -> Vec<String> {
    list.trim().split(',').filter(|r| !r.is_empty() && *r != name).map(String::from).collect()
}

fn run_script(js: &str) -> Result<()> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sweeflow-desktop.js");
    fs::write(&path, js)?;
    let conn = Connection::session()?;
    let _ = conn.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &("sweeflow-desktop",));
    let reply = conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "loadScript",
        &(path.to_string_lossy().as_ref(), "sweeflow-desktop"),
    )?;
    let id: i32 = reply.body().deserialize()?;
    conn.call_method(Some("org.kde.KWin"), format!("/Scripting/Script{id}").as_str(), Some("org.kde.kwin.Script"), "run", &())?;
    // It runs asynchronously: give it a moment before unloading.
    std::thread::sleep(std::time::Duration::from_millis(500));
    conn.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &("sweeflow-desktop",))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_peoples_rules_are_kept() {
        assert_eq!(other_rules("abc,sweeflow,def\n", RULE), ["abc", "def"]);
        assert_eq!(other_rules("", RULE), Vec::<String>::new());
        assert_eq!(other_rules("sweeflow", RULE), Vec::<String>::new());
        assert_eq!(other_rules("sweeflow,sweeflow-taskbar", "sweeflow-taskbar"), ["sweeflow"]);
    }
}
