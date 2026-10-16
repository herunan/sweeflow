//! The D-Bus side of the KDE tray widget (linux/plasmoid): everything its popup shows, as one JSON
//! property (`State`, with change signals), and the calls it makes back (`Set`, `Do`).
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

    /// One of: wake, pair, display (show the Mac's display in a window), switch <fingerprint>,
    /// forget <fingerprint>, launch <bundle id>,
    /// login on|off (open at login), quit.
    #[zbus(name = "Do")]
    fn act(&self, action: String, arg: String) {
        crate::tray::panel_do(&action, &arg);
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
    let icon = crate::launchers::icon_path(bundle_id);
    json!({
        "id": bundle_id,
        "name": name,
        "icon": if icon.exists() { format!("file://{}", icon.display()) } else { String::new() },
    })
}
