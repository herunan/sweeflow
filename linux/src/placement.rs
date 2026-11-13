//! Where Mac apps show up on the Linux desktop besides their windows: the task manager (with the
//! widget for apps running with no window, linux/plasmoid/dev.sweeflow.running) and the Global
//! Menu bar. KDE Plasma only, through its scripting interface for panels and widgets.

use anyhow::Result;
use serde::Serialize;
use zbus::blocking::Connection;

const TASKBAR_RULE: &str = "sweeflow-taskbar";
const DOCK: &str = "dev.sweeflow.dock";
const RUNNING: &str = "dev.sweeflow.running";
const APPMENU: &str = "org.kde.plasma.appmenu";

#[derive(Serialize, Clone, Copy, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    /// Plasma is running here (the rest only applies then).
    pub plasma: bool,
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
        global_menu: has(APPMENU),
    }
}

/// The Mac apps running with no window here, which the task manager can't list (it only knows
/// windows and pinned apps): a widget right after each task manager's tasks, drawn like them.
/// The task manager stops filling the panel and the widget takes the free room instead, its
/// icons at its start: so they follow the tasks, with no gap. The task manager's old setting is
/// kept in the widget's own config, and put back when switched off. (No panel properties are
/// read: on a panel whose screen is unplugged some of them crash plasmashell.)
pub fn set_running(on: bool) -> Result<()> {
    if on {
        plasma(&format!(
            "panels().forEach(function(p){{\n\
               if (p.widgets('{RUNNING}').length) return;\n\
               p.widgets().filter(function(w){{ return w.type == 'org.kde.plasma.icontasks' || w.type == 'org.kde.plasma.taskmanager'; }})\n\
                 .forEach(function(t){{\n\
                   t.currentConfigGroup = ['General'];\n\
                   var fill = t.readConfig('fill', true);\n\
                   var w = p.addWidget('{RUNNING}'); w.index = t.index + 1;\n\
                   w.currentConfigGroup = ['General'];\n\
                   w.writeConfig('taskManager', String(t.id)); w.writeConfig('restoreFill', String(fill));\n\
                   w.reloadConfig();\n\
                   t.writeConfig('fill', false); t.reloadConfig();\n\
                 }});\n\
             }});"
        ))?;
    } else {
        plasma(&format!(
            "panels().forEach(function(p){{ p.widgets('{RUNNING}').forEach(function(w){{\n\
               w.currentConfigGroup = ['General'];\n\
               var tm = w.readConfig('taskManager', ''), fill = w.readConfig('restoreFill', '');\n\
               p.widgets().forEach(function(o){{\n\
                 if (String(o.id) == String(tm) && fill !== '') {{ o.currentConfigGroup = ['General']; o.writeConfig('fill', String(fill) != 'false'); o.reloadConfig(); }}\n\
               }});\n\
               w.remove();\n\
             }}); }});"
        ))?;
    }
    Ok(())
}

/// On the first install (a marker in the config dir says it's been done), the widget for Mac
/// apps with no window, unless Mac apps are kept out of the task manager. Only once: removed by
/// hand, it stays removed.
pub fn first_install() {
    let marker = crate::config::dir().join("running-widget-added");
    if marker.exists() || crate::desktop::has_rule(TASKBAR_RULE) {
        return;
    }
    match set_running(true) {
        Ok(()) => {
            let _ = std::fs::create_dir_all(crate::config::dir());
            let _ = std::fs::write(marker, "");
        }
        Err(e) => println!("    (couldn't add the widget for Mac apps with no window: {e:#})"),
    }
}

/// The Mac Dock widget is gone (Mac apps are in the task manager, and that's it): takes every
/// one left from before off the panels (and any panel left with nothing else on it), and its
/// files out of Plasma's widgets. Mac windows are put back in the task manager too, for anyone
/// who had switched that off, and the own-desktop rule goes: neither exists any more.
pub fn retire_old_choices() {
    crate::desktop::retire_own_desktop();
    if let Err(e) = remove_dock() {
        log::debug!("removing the Mac Dock: {e:#}");
    }
    if let Some(home) = std::env::var_os("HOME") {
        let _ = std::fs::remove_dir_all(std::path::Path::new(&home).join(".local/share/plasma/plasmoids").join(DOCK));
    }
    if crate::desktop::has_rule(TASKBAR_RULE) {
        if let Err(e) = set_running(true).and_then(|()| crate::desktop::set_rule(TASKBAR_RULE, None)) {
            log::warn!("putting Mac windows back in the task manager: {e:#}");
        }
    }
}

fn remove_dock() -> Result<()> {
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

