//! Where Mac windows are on the Linux screen. Wayland doesn't tell a window where it is, so a
//! small KWin script reports it over D-Bus. The Mac then puts each window in the same place on
//! its virtual display, so the wallpaper behind it — which macOS tints sidebars and toolbars
//! with — is the same patch as behind it here.

use zbus::{blocking, interface};

use crate::net::{UiEvent, UiTx};

const SERVICE: &str = "dev.sweeflow.Client";
const PATH: &str = "/Sweeflow/Positions";
const SCRIPT_NAME: &str = "sweeflow-positions";

/// Numbers go as text: KWin sends a JS number as an int or a double depending on its value,
/// which a fixed D-Bus signature can't take both of.
const SCRIPT: &str = r#"
function report(w) {
    // Checked at report time: a new window may get its app id after KWin announces it.
    if (!String(w.resourceClass).startsWith("sweeflow-")) return;
    const g = w.frameGeometry;
    callDBus("dev.sweeflow.Client", "/Sweeflow/Positions", "dev.sweeflow.Positions", "Moved",
             String(w.resourceClass), String(w.caption), String(g.x), String(g.y), !!w.minimized);
}
function watch(w) {
    if (!w.normalWindow) return;
    w.frameGeometryChanged.connect(() => report(w));
    w.captionChanged.connect(() => report(w));
    w.minimizedChanged.connect(() => report(w));
    report(w);
}
workspace.windowList().forEach(watch);
workspace.windowAdded.connect(watch);
"#;

struct Positions {
    ui: UiTx,
}

#[interface(name = "dev.sweeflow.Positions")]
impl Positions {
    #[zbus(name = "Moved")]
    fn moved(&self, app_id: String, caption: String, x: String, y: String, minimized: bool) {
        let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else { return };
        let _ = self.ui.send(UiEvent::LinuxPosition { app_id, caption, x: x.round() as i32, y: y.round() as i32, minimized });
    }
}

/// Serves the positions interface and (re)loads the KWin script. Keep the connection alive.
pub fn start(ui: UiTx) -> zbus::Result<blocking::Connection> {
    let conn = blocking::connection::Builder::session()?
        .name(SERVICE)?
        .serve_at(PATH, Positions { ui })?
        .build()?;
    if let Err(e) = load_script(&conn) {
        log::warn!("window positions unavailable (KWin script): {e}");
    }
    Ok(conn)
}

fn load_script(conn: &blocking::Connection) -> zbus::Result<()> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sweeflow-positions.js");
    std::fs::write(&path, SCRIPT).map_err(|e| zbus::Error::Failure(e.to_string()))?;
    // A previous run's copy first (loading the same name twice fails).
    let _ = conn.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(SCRIPT_NAME,));
    let reply = conn.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript",
                                 &(path.to_string_lossy().as_ref(), SCRIPT_NAME))?;
    let id: i32 = reply.body().deserialize()?;
    conn.call_method(Some("org.kde.KWin"), format!("/Scripting/Script{id}").as_str(), Some("org.kde.kwin.Script"), "run", &())?;
    log::info!("window positions: KWin script loaded");
    Ok(())
}

