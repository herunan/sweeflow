//! The D-Bus side of the KDE tray widget (linux/plasmoid) and of Sweeflow's own window for other
//! desktops (panelwin.rs): everything they show, as one JSON
//! property (`State`, with change signals), and the calls they make back (`Set`, `Do`). Also what
//! the Global menu window asks for (`Menu`, `MenuActivate`).
//! Bus name dev.sweeflow.Client, object /dev/sweeflow/Panel, interface dev.sweeflow.Panel.

use serde_json::json;
use zbus::{blocking, interface};

const PATH: &str = "/dev/sweeflow/Panel";

pub struct Panel {
    conn: blocking::Connection,
}

struct Iface;

#[interface(name = "dev.sweeflow.Panel")]
impl Iface {
    #[zbus(property, name = "State")]
    fn state(&self) -> String {
        crate::tray::panel_state().to_string()
    }

    /// Changes settings: a JSON object with only the fields to change (as in tray::Settings).
    #[zbus(name = "Set")]
    fn set(&self, json: String) {
        crate::tray::panel_set(&json);
    }

    /// Renames a linked Mac (here only: it's what this computer calls it).
    #[zbus(name = "Rename")]
    fn rename(&self, fingerprint: String, name: String) {
        crate::tray::rename(&fingerprint, &name);
    }

    /// One of: wake, pair, stream, stop (stop streaming), show (pop the widget open), display (show the Mac's display in a window), gather (Mac windows
    /// back onto the Mac desktop), switch <fingerprint>, use <fingerprint> (a Mac just linked),
    /// forget <fingerprint>, launch <bundle id>, quit-app <bundle id>, pin|unpin <bundle id>,
    /// login on|off (open at login), square on|off (square window corners),
    /// global-menu on|off,
    /// placement (look again at those), quit.
    #[zbus(name = "Do")]
    fn act(&self, action: String, arg: String) {
        crate::tray::panel_do(&action, &arg);
    }

    /// For `sweeflow menu` (palette.rs): the menu of the Mac app whose window last had keyboard
    /// focus, as JSON {app, pid, revision, items: [{id, label, enabled, separator, checked,
    /// submenu, shortcut, children}]}. `refresh` also asks the Mac for its current state, which
    /// comes in as a later revision.
    #[zbus(name = "Menu")]
    fn menu(&self, refresh: bool) -> String {
        crate::menus::palette_menu(refresh)
    }

    /// Runs an item from `Menu` as choosing it in the Global Menu does. False when that app's
    /// menu has changed since `revision` (and the id may mean another item now): ask again.
    #[zbus(name = "MenuActivate")]
    fn menu_activate(&self, pid: i32, revision: u32, id: i32) -> bool {
        crate::menus::palette_activate(pid, revision, id)
    }
}

impl Panel {
    pub fn new() -> zbus::Result<Self> {
        let conn = blocking::connection::Builder::session()?.name("dev.sweeflow.Client")?.serve_at(PATH, Iface)?.build()?;
        Ok(Panel { conn })
    }

    /// Something the popup shows changed: tell it.
    pub fn changed(&self) {
        if let Ok(iface) = self.conn.object_server().interface::<_, Iface>(PATH) {
            let _ = zbus::block_on(iface.get().state_changed(iface.signal_emitter()));
        }
    }
}

/// The app entry the popup lists, with the icon the launchers saved (when there is one).
pub fn app(bundle_id: &str, name: &str) -> serde_json::Value {
    let url = |p: std::path::PathBuf| if p.exists() { format!("file://{}", p.display()) } else { String::new() };
    json!({
        "id": bundle_id,
        "name": name,
        // Plain everywhere, but badged in taskbars (the Sweeflow badge, when it's on).
        "icon": url(crate::launchers::original_path(bundle_id)),
        "taskIcon": url(crate::launchers::task_icon_path(bundle_id)),
    })
}
