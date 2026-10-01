//! Mac windows on a virtual desktop of their own ("Mac"), apart from the Linux ones: a KWin
//! rule places each new Mac window there (initially, so one can still be moved elsewhere).
//! Switched off, the rule and the desktop go again.

use std::{fs, process::Command};

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

/// The id of the desktop we made, kept so we only ever remove that one.
fn id_file() -> std::path::PathBuf {
    crate::config::dir().join("mac-desktop")
}

fn enable() -> Result<()> {
    let conn = Connection::session()?;
    let ours = fs::read_to_string(id_file()).unwrap_or_default();
    let mut list = desktops(&conn)?;
    if !list.iter().any(|(_, id, _)| *id == ours) {
        conn.call_method(
            Some("org.kde.KWin"),
            "/VirtualDesktopManager",
            Some("org.kde.KWin.VirtualDesktopManager"),
            "createDesktop",
            &(list.len() as u32, NAME),
        )?;
        list = desktops(&conn)?;
        let (_, id, _) = list.iter().rev().find(|(_, _, n)| n == NAME).context("desktop not created")?;
        fs::create_dir_all(crate::config::dir())?;
        fs::write(id_file(), id)?;
        log::info!("created the {NAME} desktop");
    }
    let id = fs::read_to_string(id_file())?;
    write_rule(Some(&id))?;
    // Windows already open move there too: now, and again shortly for any that were opening
    // while the rule went in (right after connecting, they all are).
    let js = format!(
        "const d = workspace.desktops.find(d => d.id == '{id}');\n\
         if (d) for (const w of workspace.windowList()) if (w.resourceClass.startsWith('sweeflow-')) w.desktops = [d];"
    );
    run_script(&js)?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    run_script(&js)
}

fn disable() -> Result<()> {
    write_rule(None)?;
    let Ok(id) = fs::read_to_string(id_file()) else { return Ok(()) };
    let conn = Connection::session()?;
    if desktops(&conn)?.iter().any(|(_, d, _)| *d == id) {
        // Any windows on it move to a neighbouring desktop.
        conn.call_method(
            Some("org.kde.KWin"),
            "/VirtualDesktopManager",
            Some("org.kde.KWin.VirtualDesktopManager"),
            "removeDesktop",
            &(id.as_str(),),
        )?;
        log::info!("removed the {NAME} desktop");
    }
    let _ = fs::remove_file(id_file());
    Ok(())
}

/// Adds (with a desktop id) or removes our rule in kwinrulesrc, keeping everyone else's.
fn write_rule(desktop: Option<&str>) -> Result<()> {
    let set = |group: &str, key: &str, value: &str| {
        Command::new("kwriteconfig6").args(["--file", "kwinrulesrc", "--group", group, "--key", key, value]).status()
    };
    let read = Command::new("kreadconfig6").args(["--file", "kwinrulesrc", "--group", "General", "--key", "rules"]).output()?;
    let mut rules: Vec<String> = String::from_utf8_lossy(&read.stdout)
        .trim()
        .split(',')
        .filter(|r| !r.is_empty() && *r != RULE)
        .map(String::from)
        .collect();
    match desktop {
        Some(id) => {
            for (k, v) in [
                ("Description", "Mac apps on their own desktop (sweeflow)"),
                ("wmclass", "^sweeflow-"),
                ("wmclassmatch", "3"),
                ("wmclasscomplete", "false"),
                ("desktops", id),
                // Apply initially: placed there when opened, free to move afterwards.
                ("desktopsrule", "3"),
            ] {
                set(RULE, k, v)?;
            }
            rules.push(RULE.into());
        }
        None => {
            for k in ["Description", "wmclass", "wmclassmatch", "wmclasscomplete", "desktops", "desktopsrule"] {
                Command::new("kwriteconfig6").args(["--file", "kwinrulesrc", "--group", RULE, "--key", k, "--delete"]).status()?;
            }
        }
    }
    set("General", "rules", &rules.join(","))?;
    set("General", "count", &rules.len().to_string())?;
    Command::new("qdbus6").args(["org.kde.KWin", "/KWin", "reconfigure"]).status()?;
    Ok(())
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
