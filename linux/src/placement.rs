//! Where Mac apps show up on the Linux desktop besides their windows: the task manager, the Mac
//! Dock widget (linux/plasmoid/dev.sweeflow.dock) and the Global Menu bar. KDE Plasma only: a
//! KWin rule for the task manager, Plasma's scripting interface for panels and widgets.

use anyhow::{Context, Result};
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

/// Adds the Mac Dock: `own` as a floating dock panel of its own at the bottom (hiding when a
/// window gets near), otherwise into the panel with the task manager, after it. Either way
/// Plasma's edit mode opens, so it can be dragged where it belongs.
pub fn add_dock(own: bool) -> Result<()> {
    let script = if own {
        format!(
            "var p = new Panel('org.kde.panel');\n\
             p.location = 'bottom'; p.height = 64; p.lengthMode = 'fit'; p.alignment = 'center';\n\
             p.floating = true; p.hiding = 'dodgewindows';\n\
             p.addWidget('{DOCK}');\n\
             print('ok');"
        )
    } else {
        format!(
            "var ps = panels().filter(function(p){{ return p.widgets('org.kde.plasma.icontasks').length || p.widgets('org.kde.plasma.taskmanager').length; }});\n\
             var p = ps.filter(function(p){{ return p.screen == 0; }})[0] || ps[0] || panels()[0];\n\
             if (!p) {{ print('no panel'); }} else {{\n\
               var tasks = p.widgets('org.kde.plasma.icontasks')[0] || p.widgets('org.kde.plasma.taskmanager')[0];\n\
               var w = p.addWidget('{DOCK}');\n\
               // Right after the task manager, rather than at the end.\n\
               if (tasks) {{\n\
                 var ids = p.widgetIds.filter(function(id){{ return id != w.id; }});\n\
                 ids.splice(ids.indexOf(tasks.id) + 1, 0, w.id);\n\
                 p.currentConfigGroup = ['General']; p.writeConfig('AppletOrder', ids.join(';'));\n\
               }}\n\
               print('ok');\n\
             }}"
        )
    };
    let out = plasma(&script)?;
    anyhow::ensure!(out.contains("ok"), "couldn't add the Mac Dock: {out}");
    edit_mode();
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

/// The Global Menu bar (the Mac apps' menus, like the Mac's menu bar): a slim panel at the top
/// with the Global Menu widget. Off removes the widget (and its panel, if that's all it held).
pub fn set_global_menu(on: bool) -> Result<()> {
    let script = if on {
        format!(
            "var has = panels().some(function(p){{ return p.widgets('{APPMENU}').length; }});\n\
             if (!has) {{ var p = new Panel('org.kde.panel'); p.location = 'top'; p.height = 28; p.lengthMode = 'fit';\n\
               p.alignment = 'left'; p.floating = false; p.addWidget('{APPMENU}'); }}"
        )
    } else {
        format!(
            "panels().forEach(function(p){{\n\
               var m = p.widgets('{APPMENU}');\n\
               if (!m.length) return;\n\
               if (p.widgets().length == m.length) p.remove(); else m.forEach(function(w){{ w.remove(); }});\n\
             }});"
        )
    };
    plasma(&script).map(|_| ()).context("Global Menu")
}

/// Plasma's edit mode, to drag the new widget where it belongs.
fn edit_mode() {
    let _ = Connection::session().and_then(|c| {
        c.call_method(
            Some("org.kde.plasmashell"),
            "/PlasmaShell",
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &("org.kde.PlasmaShell", "editMode", zvariant::Value::from(true)),
        )
        .map(|_| ())
    });
}
