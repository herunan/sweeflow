//! Where Mac apps show up on the Linux desktop besides their windows: the task manager, the Mac
//! Dock widget (linux/plasmoid/dev.sweeflow.dock) and the Global Menu bar. KDE Plasma only: a
//! KWin rule for the task manager, Plasma's scripting interface for panels and widgets.

use anyhow::Result;
use serde::Serialize;
use zbus::blocking::Connection;

const TASKBAR_RULE: &str = "sweeflow-taskbar";
const DOCK: &str = "dev.sweeflow.dock";
const APPMENU: &str = "org.kde.plasma.appmenu";

#[derive(Serialize, Clone, Copy, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    /// Plasma is running here (the rest only applies then).
    pub plasma: bool,
    /// Mac windows listed in the task manager (no rule hiding them).
    pub taskbar: bool,
    /// A Mac Dock widget is on some panel.
    pub dock: bool,
    /// A Global Menu widget is on some panel.
    pub global_menu: bool,
}

/// Runs a Plasma desktop script and returns what it printed.
fn plasma(script: &str) -> Result<String> {
    let conn = Connection::session()?;
    let reply = conn.call_method(
        Some("org.kde.plasmashell"),
        "/PlasmaShell",
        Some("org.kde.PlasmaShell"),
        "evaluateScript",
        &(script,),
    )?;
    Ok(reply.body().deserialize::<String>()?)
}

/// Looks at the current set-up (a few D-Bus round trips: call off the UI thread).
pub fn query() -> Placement {
    let widgets = plasma(
        "var t = []; panels().forEach(function(p){ p.widgets().forEach(function(w){ t.push(w.type); }); }); print(t.join(','));",
    );
    let Ok(widgets) = widgets else { return Placement::default() };
    let has = |id: &str| widgets.split(',').any(|w| w.trim() == id);
    Placement {
        plasma: true,
        taskbar: !crate::desktop::has_rule(TASKBAR_RULE),
        dock: has(DOCK),
        global_menu: has(APPMENU),
    }
}

/// Mac windows in the task manager, or not (a KWin rule skipping it for them; the Mac display
/// view stays, like any window you opened yourself).
pub fn set_taskbar(on: bool) -> Result<()> {
    if on {
        crate::desktop::set_rule(TASKBAR_RULE, None)
    } else {
        crate::desktop::set_rule(
            TASKBAR_RULE,
            Some(&[
                ("Description", "Mac apps out of the task manager (sweeflow)"),
                ("wmclass", "^sweeflow-(?!display$)"),
                ("wmclassmatch", "3"),
                ("wmclasscomplete", "false"),
                ("skiptaskbar", "true"),
                ("skiptaskbarrule", "2"),
            ]),
        )
    }
}

/// Adds the Mac Dock: a floating dock panel of its own at the bottom, centred, which gets out of
/// the way when a window comes near it.
pub fn add_dock() -> Result<()> {
    let out = plasma(&format!(
        "var p = new Panel('org.kde.panel');\n\
         p.location = 'bottom'; p.height = 64; p.lengthMode = 'fit'; p.alignment = 'center';\n\
         p.floating = true; p.hiding = 'dodgewindows';\n\
         p.addWidget('{DOCK}');\n\
         print('ok');"
    ))?;
    anyhow::ensure!(out.contains("ok"), "couldn't add the Mac Dock: {out}");
    Ok(())
}

/// Removes every Mac Dock, and any panel left with nothing else on it.
pub fn remove_dock() -> Result<()> {
    plasma(&format!(
        "panels().forEach(function(p){{\n\
           var docks = p.widgets('{DOCK}');\n\
           if (!docks.length) return;\n\
           if (p.widgets().length == docks.length) p.remove(); else docks.forEach(function(w){{ w.remove(); }});\n\
         }});"
    ))?;
    Ok(())
}

/// The id of the top bar we made for the Global Menu (only that one is ever removed).
fn menu_panel_file() -> std::path::PathBuf {
    crate::config::dir().join("global-menu-panel")
}

/// The Global Menu (the Mac apps' menus, like the Mac's menu bar). On: into the top panel there
/// already is, or else a slim bar of its own at the top, centred. Off: the widget goes, and the
/// bar too if we made it.
pub fn set_global_menu(on: bool) -> Result<()> {
    let ours = std::fs::read_to_string(menu_panel_file()).unwrap_or_default().trim().to_string();
    if on {
        let out = plasma(&format!(
            "if (panels().some(function(p){{ return p.widgets('{APPMENU}').length; }})) print('has');\n\
             else {{\n\
               var top = panels().filter(function(p){{ return p.location == 'top'; }})[0];\n\
               if (top) {{ top.addWidget('{APPMENU}'); print('added'); }}\n\
               else {{ var p = new Panel('org.kde.panel'); p.location = 'top'; p.height = 28; p.lengthMode = 'fit';\n\
                 p.alignment = 'center'; p.floating = false; p.addWidget('{APPMENU}'); print('made ' + p.id); }}\n\
             }}"
        ))?;
        if let Some(id) = out.trim().strip_prefix("made ") {
            std::fs::write(menu_panel_file(), id)?;
        }
    } else {
        plasma(&format!(
            "panels().forEach(function(p){{\n\
               var m = p.widgets('{APPMENU}');\n\
               if (!m.length) return;\n\
               if (p.id == '{ours}' && p.widgets().length == m.length) p.remove(); else m.forEach(function(w){{ w.remove(); }});\n\
             }});"
        ))?;
        let _ = std::fs::remove_file(menu_panel_file());
    }
    Ok(())
}

