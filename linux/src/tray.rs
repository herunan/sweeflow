//! The tray icon: streaming on and off, which Mac apps, and the features, with the same switches
//! as the Mac's menu-bar item. The Mac keeps the settings; changing them on either side changes
//! them on both. Stop (the choice kept across restarts) lets go of the Mac altogether, Start takes
//! it back; Pause keeps the connection and only stops the windows coming here, until Resume.

use std::sync::{LazyLock, Mutex, OnceLock};

use ksni::{
    blocking::{Handle, TrayMethods},
    menu::StandardItem,
    MenuItem, ToolTip,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    net::NetCmd,
    proto::{t, W},
};

/// As the Mac sends them (Settings.Wire in Settings.swift).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub enabled: bool,
    pub exclude: Vec<String>,
    pub sound: bool,
    pub dim: bool,
    pub clipboard: bool,
    pub drag_drop: bool,
    pub launcher: bool,
    pub quality: String,
    /// Mac windows on a desktop of their own (see desktop.rs).
    #[serde(default = "yes")]
    pub desktop: bool,
    /// The MacBook stays awake with its lid closed while it streams (its rule, LidSleepRule.swift).
    /// Asking for it brings up a password prompt on the Mac; what the Mac sends back says whether
    /// it was given.
    #[serde(default)]
    pub lid: bool,
    /// The Mac has a lid (a MacBook): only then is `lid` offered. Older Macs don't say: none.
    #[serde(default)]
    pub has_lid: bool,
    /// The Mac's password prompt for `lid` is up: the switch waits for it.
    #[serde(default)]
    pub lid_asking: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    /// The Mac's defaults (Config in main.swift): shown before it has ever been reached.
    fn default() -> Self {
        Settings {
            enabled: true,
            exclude: Vec::new(),
            sound: true,
            dim: false,
            clipboard: true,
            drag_drop: true,
            launcher: true,
            quality: "sharp".into(),
            desktop: true,
            lid: false,
            has_lid: false,
            lid_asking: false,
        }
    }
}

#[derive(Default)]
struct State {
    /// The Mac's name while connected.
    mac: Option<String>,
    /// Known while connected.
    settings: Option<Settings>,
    /// (bundle id, name) of the Mac's apps.
    apps: Vec<(String, String)>,
    /// Running Mac apps: 3 = a window shown here, 2 = only minimised ones, 1 = no window.
    running: std::collections::HashMap<String, u8>,
    /// The settings as last seen from the Mac.
    last: Option<Settings>,
    /// Settings changed while the Mac was away (only those that differ from `last`): shown over
    /// them, and sent once it's back.
    pending: serde_json::Map<String, serde_json::Value>,
    /// Pinned Mac apps (bundle ids, in order): first in the tray's Apps page, kept in the Dock.
    pinned: Vec<String>,
    /// Where Mac apps show up (task manager, Global Menu), as last looked at.
    placement: crate::placement::Placement,
    /// When Sweeflow was last opened from the app menu (ms since the epoch): the widget pops up.
    show_at: u64,
    /// The Mac stopped this computer from its own menu (it refuses until allowed there again).
    mac_refused: bool,
    /// The Mac is streaming to another computer (never named, for privacy): this one waits,
    /// looking again now and then, and streams once it's free. Start can't be chosen meanwhile.
    elsewhere: bool,
}

static STATE: LazyLock<Mutex<State>> =
    LazyLock::new(|| {
        Mutex::new(State {
            last: load_last(),
            pending: load_pending(),
            pinned: load_pinned(),
            apps: load_apps(),
            ..Default::default()
        })
    });
/// Wakes the tray's thread to redraw (its blocking handle can't be used from async code).
static REFRESH: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();
static NET: OnceLock<UnboundedSender<NetCmd>> = OnceLock::new();

/// The connection to the Mac is being made (a Mac is linked): the tray's actions can reach it.
pub fn set_net(net: UnboundedSender<NetCmd>) {
    let _ = NET.set(net);
    refresh();
}

/// No Mac linked yet: the tray offers only "Link a Mac…".
fn unpaired() -> bool {
    NET.get().is_none() && crate::config::Config::load_or_default().is_ok_and(|c| c.host.is_empty())
}

/// Shows the tray icon (or serves the KDE widget). Before a Mac is linked too.
pub fn start() {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = REFRESH.set(tx);
    // The keys the menu and panels show, as the user changes them.
    crate::keys::watch();
    // No tray to show the icon in (plain Hyprland, a bar without one): say once that Sweeflow
    // runs and how to open it, or nothing tells anyone.
    std::thread::spawn(|| {
        // A bar may come up a little after Sweeflow at login.
        std::thread::sleep(std::time::Duration::from_secs(15));
        if !widget_installed() && !tray_host_present() {
            tell_no_tray();
        }
    });
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let panel = crate::panel::Panel::new().map_err(|e| log::warn!("tray widget's D-Bus service: {e}")).ok();
            refresh_placement();
            // With the KDE widget installed, it is the tray icon; the plain one is for elsewhere.
            let handle: Option<Handle<Tray>> = if widget_installed() {
                None
            } else {
                // The bar that shows tray icons (Omarchy's, waybar, GNOME's extension) may come up a
                // moment after Sweeflow at login, or restart: the icon waits for it and shows then
                // (a refused registration was final: no icon until Sweeflow restarted).
                match Tray.assume_sni_available(true).spawn() {
                    Ok(h) => Some(h),
                    Err(e) => {
                        log::warn!("tray icon: {e}");
                        None
                    }
                }
            };
            // Whether the icon is to be shown again once things are quiet (see TWIN).
            let mut again = false;
            loop {
                let got = if again { rx.recv_timeout(std::time::Duration::from_secs(2)) } else { rx.recv().map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected) };
                match got {
                    Ok(()) => {
                        // Several changes at once: one redraw.
                        while rx.try_recv().is_ok() {}
                        if let Some(h) = &handle {
                            h.update(|_| {});
                        }
                        if let Some(p) = &panel {
                            p.changed();
                        }
                        again = handle.is_some();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        again = false;
                        ICON_TWIN.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                        if let Some(h) = &handle {
                            h.update(|_| {});
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .unwrap();
}

/// Whether anything shows tray icons here (a StatusNotifier watcher on the session bus).
fn tray_host_present() -> bool {
    let Ok(bus) = zbus::blocking::Connection::session() else { return true };
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&bus) else { return true };
    dbus.name_has_owner("org.kde.StatusNotifierWatcher".try_into().unwrap()).unwrap_or(true)
}

/// A desktop notification that Sweeflow is running and which key opens it (when there's no tray).
fn tell_no_tray() {
    let key = crate::keys::current().open.map(|k| k.shown(false, "Super"));
    let body = match key {
        Some(k) => format!("{k} opens it; it's in your app launcher too."),
        None => "Open it from your app launcher (or run `sweeflow open`).".to_string(),
    };
    let Ok(bus) = zbus::blocking::Connection::session() else { return };
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value> = std::collections::HashMap::new();
    let r = bus.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &("Sweeflow", 0u32, "sweeflow", "Sweeflow is running", body.as_str(), Vec::<&str>::new(), hints, 10_000i32),
    );
    match r {
        Ok(_) => log::info!("no tray here: told how to open Sweeflow ({body})"),
        Err(e) => log::info!("no tray here, and no notifications either: {e}"),
    }
}

/// Tells the tray (and widget) to redraw: something they show changed.
/// The Mac's whole screen is up in a window here (Settings' Troubleshoot row: Show / Hide).
static DISPLAY_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_display_shown(shown: bool) {
    DISPLAY_SHOWN.store(shown, std::sync::atomic::Ordering::Relaxed);
    refresh();
}

pub fn refresh() {
    if let Some(tx) = REFRESH.get() {
        let _ = tx.send(());
    }
}

pub fn connected(mac: Option<String>) {
    let mut s = STATE.lock().unwrap();
    if mac.is_some() {
        s.mac_refused = false;
    }
    if mac.is_none() {
        s.settings = None;
        s.running.clear();
    }
    let gone = mac.is_none();
    s.mac = mac;
    drop(s);
    if gone {
        crate::update::set_mac_version(None);
    }
    refresh();
}

pub fn set_settings(json: &[u8]) {
    match serde_json::from_slice::<Settings>(json) {
        Ok(mut new) => {
            log::info!("settings: {new:?}");
            // Changed here while the Mac was away: those win, and go to the Mac now.
            let pending = std::mem::take(&mut STATE.lock().unwrap().pending);
            let send = !pending.is_empty();
            if send {
                log::info!("applying the changes made while the Mac was away: {pending:?}");
                if let Ok(serde_json::Value::Object(mut now)) = serde_json::to_value(&new) {
                    now.extend(pending);
                    match serde_json::from_value(serde_json::Value::Object(now)) {
                        Ok(merged) => new = merged,
                        Err(e) => log::warn!("changes made while away: {e}"),
                    }
                }
                save_pending(&Default::default());
            }
            {
                let mut s = STATE.lock().unwrap();
                save_last(&new);
                s.last = Some(new.clone());
                s.settings = Some(new);
            }
            if send {
                change(|_| {});
            }
            refresh();
            crate::net::launcher_setting_changed();
        }
        Err(e) => log::warn!("bad settings from the Mac: {e}"),
    }
}

pub fn set_running(running: Vec<(String, u8)>) {
    STATE.lock().unwrap().running = running.into_iter().collect();
    refresh();
}

pub fn set_apps(apps: Vec<(String, String)>) {
    save_apps(&apps);
    STATE.lock().unwrap().apps = apps;
    refresh();
}

fn get<T>(f: impl FnOnce(&Settings) -> T, unknown: T) -> T {
    STATE.lock().unwrap().settings.as_ref().map_or(unknown, f)
}

/// A Mac app's name, from its bundle id (the id itself if it isn't known).
pub fn app_name(bid: &str) -> String {
    let s = STATE.lock().unwrap();
    s.apps.iter().find(|(b, _)| b == bid).map(|(_, n)| n.clone()).unwrap_or_else(|| bid.to_string())
}

pub fn allows_clipboard() -> bool {
    get(|s| s.enabled && s.clipboard, true)
}

pub fn streaming() -> bool {
    get(|s| s.enabled, true)
}

pub fn allows_drag() -> bool {
    get(|s| s.enabled && s.drag_drop, true)
}

pub fn launcher_wanted() -> bool {
    get(|s| s.enabled && s.launcher, true)
}

/// Changes a setting: shown straight away, and sent to the Mac (which confirms with the new
/// settings for both sides). With the Mac away (or streaming not known yet), kept and sent
/// once it's back.
fn change(f: impl FnOnce(&mut Settings)) {
    let mut state = STATE.lock().unwrap();
    if let Some(s) = state.settings.as_mut() {
        f(s);
        let json = serde_json::to_vec(s).unwrap_or_default();
        let streaming = s.enabled;
        drop(state);
        // The UI first, then the Mac (see net::tell_streaming).
        crate::net::tell_streaming(streaming);
        if let Some(net) = NET.get() {
            let _ = net.send(NetCmd::Control(W::new(t::SET_SETTINGS).bytes(&json).finish()));
        }
        return;
    }
    let mut wanted = away_view(&state);
    f(&mut wanted);
    let mac = serde_json::to_value(state.last.clone().unwrap_or_default()).unwrap_or_default();
    if let (serde_json::Value::Object(mac), serde_json::Value::Object(wanted)) = (mac, serde_json::to_value(wanted).unwrap_or_default()) {
        state.pending = wanted.into_iter().filter(|(k, v)| mac.get(k) != Some(v)).collect();
    }
    save_pending(&state.pending);
    log::info!("the Mac is away: will apply {:?} once it's back", state.pending);
    drop(state);
    refresh();
}

/// The plain tray icon, for desktops without the Plasma widget (GNOME, Hyprland). Their menus
/// close after every click and can't show running apps or rename a Mac, so a click opens
/// Sweeflow's own window (panelwin.rs) and the menu is only a short fallback.
struct Tray;

impl ksni::Tray for Tray {
    fn activate(&mut self, _x: i32, _y: i32) {
        open_panel();
    }

    fn id(&self) -> String {
        "sweeflow".into()
    }

    fn title(&self) -> String {
        "Sweeflow".into()
    }

    /// The logo with the dot for where the Mac stands, as Plasma's widget draws it (installed by
    /// `sweeflow install`, one icon per state). Not an overlay icon: bars that get one draw the
    /// pair themselves, and not every theme has the emblems.
    fn icon_name(&self) -> String {
        let state = STATE.lock().unwrap();
        let name = tray_icon_of(state_dot(status_short(standing(&state)).as_str()));
        if ICON_TWIN.load(std::sync::atomic::Ordering::Relaxed) { name + TWIN } else { name }
    }

    /// Where that icon is, for bars that haven't noticed them in the icon theme (one that
    /// started before the folder existed, as on a first install).
    fn icon_theme_path(&self) -> String {
        std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".local/share/icons/hicolor/symbolic/apps").to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if unpaired() { "No Mac linked yet".into() } else { status() };
        ToolTip { title: "Sweeflow".into(), description, ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        if unpaired() {
            return vec![
                StandardItem { label: "No Mac linked yet".into(), enabled: false, ..Default::default() }.into(),
                StandardItem {
                    label: "Link a Mac…".into(),
                    icon_name: menu_icon("list-add-symbolic"),
                    activate: Box::new(|_| open_wizard()),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Sweeflow".into(),
                        activate: Box::new(|_| {
                        quit();
                    }),
                    ..Default::default()
                }
                .into(),
            ];
        }
        let state = STATE.lock().unwrap();
        let status = status_of(&state).replace('_', "__");
        // The keys' column on GNOME (see with_key): as far right as the status line reaches.
        let column = key_column(&status);
        let dot = state_dot(status_short(standing(&state)).as_str());
        let actions = stream_actions(standing(&state));
        let elsewhere = state.elsewhere;
        drop(state);
        // Each item shows Sweeflow's key for it as set (the Shortcuts window), as every window does.
        let keys = crate::keys::own_as_keys();
        // Where streaming stands, and what can be done about it.
        // The status line leads with the same coloured dot the window gives the Mac in use.
        let mut items: Vec<MenuItem<Self>> = vec![StandardItem { label: status, enabled: false, icon_name: dot.into(), ..Default::default() }.into()];
        // How the Mac in use is reached ("Local: 192.168.1.20"), under it.
        let cfg = crate::config::Config::load_or_default().unwrap_or_default();
        if let Some(m) = cfg.macs.iter().find(|m| m.fingerprint == cfg.host_fingerprint) {
            let at = reached(m, &cfg.host_fingerprint);
            let words = crate::net::route_words(crate::net::route_kind(&at), &address_only(&at));
            if !words.is_empty() {
                items.push(StandardItem { label: words.replace('_', "__"), enabled: false, ..Default::default() }.into());
            }
        }
        for &action in actions {
            if elsewhere && action == "start" {
                items.push(StandardItem { label: start_hint(), enabled: false, ..Default::default() }.into());
                continue;
            }
            let key = match action {
                "pause" | "resume" => &keys.pause,
                _ => &keys.stop,
            };
            items.push(
                StandardItem {
                    label: with_key(action_label(action), key, column),
                    shortcut: key.as_ref().map(|k| k.menu()).unwrap_or_default(),
                    activate: Box::new(move |_| panel_do(action, "")),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: with_key("Open Sweeflow", &keys.open, column),
                shortcut: keys.open.as_ref().map(|k| k.menu()).unwrap_or_default(),
                activate: Box::new(|_| open_panel()),
                ..Default::default()
            }
            .into(),
        );
        // (The Global menu opens with Cmd+Opt+U in a Mac window, on every desktop: no item here.)
        items.push(MenuItem::Separator);
        // Updates: a newer release, once seen, or a look for one. Either opens the update window.
        let update_label = match crate::update::available() {
            Some(_) => String::from("Update available"),
            None => "Check for updates".into(),
        };
        items.push(
            StandardItem {
                label: update_label.replace('_', "__"),
                activate: Box::new(|_| open_update_window()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit Sweeflow".into(),
                activate: Box::new(|_| {
                    quit();
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Where the desktop lets keyboard shortcuts be changed (Sweeflow's among them): Plasma's
/// System Settings at Shortcuts, GNOME's Settings at Keyboard. Hyprland has no such place (its
/// config file; Sweeflow adds its keys while it runs), so nothing opens there.
pub fn shortcut_settings_place() -> Option<&'static [&'static str]> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktop.contains("KDE") {
        Some(&["systemsettings", "kcm_keys"])
    } else if desktop.contains("GNOME") {
        Some(&["gnome-control-center", "keyboard"])
    } else if desktop.contains("Cinnamon") {
        Some(&["cinnamon-settings", "keyboard"])
    } else {
        None
    }
}

pub(crate) fn open_shortcut_settings() {
    if let Some(cmd) = shortcut_settings_place() {
        if let Err(e) = std::process::Command::new(cmd[0]).args(&cmd[1..]).spawn() {
            log::warn!("opening the shortcut settings ({}): {e}", cmd[0]);
        }
    }
}

/// An icon for a menu entry (symbolic: each desktop draws it in its menu's text colour).
fn menu_icon(name: &str) -> String {
    name.into()
}

/// A menu item's text, with its key after it where the tray doesn't show a menu item's shortcut
/// (everywhere but Plasma, which shows it at the right).
fn with_key(label: &str, key: &Option<crate::keys::Key>, column: f32) -> String {
    match key {
        // Only Plasma's tray draws a menu item's shortcut; GNOME's, Cinnamon's and Omarchy's
        // bar don't, so there the key goes in the text.
        Some(k) if !std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().contains("KDE") => {
            // Lined up in a column at the end of the rows, by tabs (key_column).
            // Omarchy's bar draws its menus at a fixed width: tabs pushed the key out of sight.
            if crate::hyprland::running() {
                return format!("{label}  {}", k.shown(false, "Super"));
            }
            let tabs = (column - tab_stop(text_width(label))) as usize + 1;
            format!("{label}{}{}", "\t".repeat(tabs), k.shown(false, "Super"))
        }
        _ => label.to_string(),
    }
}

/// GNOME's menu has a tab stop every eight spaces (32 px in Adwaita Sans; 24 px in Ubuntu Sans,
/// whose space is narrower), and a tab needs about 8 px of room before the next one (measured):
/// the first stop a tab after text this wide reaches.
fn tab() -> f32 {
    // The space rounded to whole pixels, as the menu has it (Adwaita Sans's 4.1 px space gives
    // 32 px stops, not 33).
    MENU_FONT.as_ref().and_then(|f| f.width(" ")).map(|w| 8.0 * w.round()).filter(|w| *w > 8.0).unwrap_or(32.0)
}
fn tab_stop(width: f32) -> f32 {
    ((width + 8.0) / tab()).ceil()
}

/// The tab stop the keys start at, so they end where the rows do: the status line (with its dot
/// after it) is the widest; never before the first stop after "Open Sweeflow".
fn key_column(status: &str) -> f32 {
    const DOT: f32 = 22.0;
    let key = text_width("Super+Alt+Y");
    // One stop further than where the status line ends: set apart from the item's text.
    let end = ((text_width(status) + DOT - key) / tab()).floor() + 1.0;
    end.max(tab_stop(text_width("Open Sweeflow")))
}

/// How wide a menu item's text is in the desktop's menu font: measured with that font where
/// it can be found, else about as wide as in GNOME's (Adwaita Sans / Cantarell, 11 pt).
fn text_width(text: &str) -> f32 {
    if let Some(w) = MENU_FONT.as_ref().and_then(|f| f.width(text)) {
        return w;
    }
    text.chars()
        .map(|c| match c {
            ' ' => 4.2,
            'i' | 'l' | 'j' | 't' | 'f' | 'r' | 'I' => 4.5,
            'm' | 'w' => 11.0,
            'M' | 'W' => 13.0,
            c if c.is_uppercase() => 9.5,
            _ => 7.6,
        })
        .sum::<f32>()
        * 1.08
}

/// The font the tray's menu is drawn in, and its size in pixels.
struct MenuFont {
    data: Vec<u8>,
    px: f32,
}

impl MenuFont {
    /// Ubuntu's shell theme draws its menus in Ubuntu Sans whatever the interface font is;
    /// elsewhere (GNOME, Cinnamon) the interface font setting, e.g. "Adwaita Sans 11".
    fn find() -> Option<Self> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
        let schema = if desktop.contains("cinnamon") { "org.cinnamon.desktop.interface" } else { "org.gnome.desktop.interface" };
        let get = |key: &str| {
            let out = std::process::Command::new("gsettings").args(["get", schema, key]).output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string())
        };
        let setting = get("font-name")?;
        let (family, pt) = setting.rsplit_once(' ').and_then(|(f, p)| Some((f.trim().to_string(), p.parse::<f32>().ok()?)))?;
        let family = if desktop.contains("ubuntu") { "Ubuntu Sans".to_string() } else { family };
        let scale = get("text-scaling-factor").and_then(|s| s.parse::<f32>().ok()).unwrap_or(1.0);
        let out = std::process::Command::new("fc-match").args(["-f", "%{file}", &family]).output().ok()?;
        let file = String::from_utf8_lossy(&out.stdout).to_string();
        let data = std::fs::read(&file).ok()?;
        ttf_parser::Face::parse(&data, 0).ok()?;
        log::info!("tray menu: keys lined up for {family} {pt} pt ({file})");
        Some(MenuFont { data, px: pt * 96.0 / 72.0 * scale })
    }

    fn width(&self, text: &str) -> Option<f32> {
        let face = ttf_parser::Face::parse(&self.data, 0).ok()?;
        let units: u32 = text
            .chars()
            .map(|c| face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g)).unwrap_or(face.units_per_em() / 2) as u32)
            .sum();
        Some(units as f32 * self.px / face.units_per_em() as f32)
    }
}

static MENU_FONT: LazyLock<Option<MenuFont>> = LazyLock::new(MenuFont::find);

/// The coloured dots for where the Mac in use stands, as Sweeflow's window and widget draw them
/// (name, colour, a ring): violet streaming (a ring paused), green available, orange busy or
/// locked, a grey ring not online or not known. Installed as named icons (setup::write_icons).
pub const STATE_DOTS: [(&str, &str, bool); 5] = [
    ("sweeflow-dot-streaming", "#8b5cf6", false),
    ("sweeflow-dot-paused", "#8b5cf6", true),
    ("sweeflow-dot-available", "#30a46c", false),
    ("sweeflow-dot-busy", "#e8890c", false),
    ("sweeflow-dot-offline", "#9a96a3", true),
];

/// The suffix of each tray icon's twin, the same picture under a second name. Ubuntu's tray
/// extension can miss a change of icon (offline and back within seconds: it went on showing
/// offline while streaming), and an icon sent again under the same name changes nothing. So a
/// moment after each change the icon is sent again as its twin, and the tray loads it afresh.
pub const TWIN: &str = "-again";
static ICON_TWIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The tray icon with that dot (setup.rs writes them): "sweeflow-tray-streaming"…
pub fn tray_icon_of(dot: &str) -> String {
    dot.replacen("sweeflow-dot-", "sweeflow-tray-", 1)
}

/// The dot's icon name for the status line.
fn state_dot(status: &str) -> &'static str {
    match status {
        "Streaming" => STATE_DOTS[0].0,
        "Paused" => STATE_DOTS[1].0,
        "Available" => STATE_DOTS[2].0,
        "Streaming elsewhere" | "Locked" => STATE_DOTS[3].0,
        _ => STATE_DOTS[4].0,
    }
}

fn quit() {
    crate::net::goodbye();
    // Through systemd, which would otherwise start it again (under the service, this stop is what
    // ends us). Started any other way (KRunner, a terminal), the stop is a no-op, so exit ourselves.
    let _ = std::process::Command::new("systemctl").args(["--user", "stop", "sweeflow"]).status();
    std::process::exit(0);
}

/// A widget of the desktop's shell shows Sweeflow's icon and panel, so Sweeflow shows no tray
/// icon or window of its own: the KDE tray widget (linux/plasmoid), installed by `sweeflow
/// install` on Plasma.
pub fn widget_installed() -> bool {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return false };
    home.join(".local/share/plasma/plasmoids/dev.sweeflow.panel/metadata.json").exists()
}

fn pinned_file() -> std::path::PathBuf {
    crate::config::dir().join("pinned")
}

fn load_pinned() -> Vec<String> {
    std::fs::read_to_string(pinned_file()).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

fn save_pinned(pinned: &[String]) {
    let _ = std::fs::write(pinned_file(), pinned.join("\n"));
}

/// The settings as they'll be once the Mac is back: as it last had them, with the changes since.
fn away_view(state: &State) -> Settings {
    let mut now = serde_json::to_value(state.last.clone().unwrap_or_default()).unwrap_or_default();
    if let serde_json::Value::Object(now) = &mut now {
        now.extend(state.pending.clone());
    }
    serde_json::from_value(now).unwrap_or_else(|_| state.last.clone().unwrap_or_default())
}

fn apps_file() -> std::path::PathBuf {
    crate::config::dir().join("apps.json")
}

/// The Mac's apps as last listed: shown (and launchable) while it's away.
fn load_apps() -> Vec<(String, String)> {
    serde_json::from_slice(&std::fs::read(apps_file()).unwrap_or_default()).unwrap_or_default()
}

fn save_apps(apps: &[(String, String)]) {
    if let Ok(json) = serde_json::to_vec(apps) {
        let _ = std::fs::write(apps_file(), json);
    }
}

fn pending_file() -> std::path::PathBuf {
    crate::config::dir().join("pending-settings.json")
}

fn load_pending() -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_slice(&std::fs::read(pending_file()).unwrap_or_default()).unwrap_or_default()
}

fn save_pending(p: &serde_json::Map<String, serde_json::Value>) {
    if p.is_empty() {
        let _ = std::fs::remove_file(pending_file());
    } else if let Ok(json) = serde_json::to_vec(p) {
        let _ = std::fs::write(pending_file(), json);
    }
}

fn last_file() -> std::path::PathBuf {
    crate::config::dir().join("last-settings.json")
}

fn load_last() -> Option<Settings> {
    serde_json::from_slice(&std::fs::read(last_file()).ok()?).ok()
}

fn save_last(s: &Settings) {
    if let Ok(json) = serde_json::to_vec(s) {
        let _ = std::fs::write(last_file(), json);
    }
}

/// Where streaming stands: what the status line says.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Standing {
    /// Stop was chosen (no connection is held).
    pub stopped: bool,
    /// The Mac is streaming to another computer (and turns this one away until it stops).
    pub elsewhere: bool,
    /// Connected, and the Mac's settings known.
    pub connected: bool,
    /// Not paused (Pause / Resume).
    pub enabled: bool,
    /// The Mac's screen is locked.
    pub locked: bool,
    /// The Mac stopped this computer from its own menu (it refuses until allowed there again).
    pub refused: bool,
    /// The Mac can be reached; None while not known yet.
    pub online: Option<bool>,
}

/// The same status where the Mac's name stands right above it (the widget and Sweeflow's own
/// window): without the name, which only repeated it.
pub fn status_short(s: Standing) -> String {
    if let Standing { stopped: false, elsewhere: true, .. } = s {
        return "Streaming elsewhere".into();
    }
    // The same words as the Mac's line in the list of Macs.
    match s {
        Standing { stopped: true, online: Some(true), elsewhere: true, .. } => "Streaming elsewhere",
        Standing { stopped: true, online: Some(true), locked: true, .. } => "Locked",
        Standing { stopped: true, online: Some(true), .. } => "Available",
        Standing { stopped: true, online: Some(false), .. } => "Not online",
        Standing { stopped: true, .. } => "Stopped",
        Standing { connected: true, locked: true, .. } => "Locked",
        Standing { connected: true, enabled: false, .. } => "Paused",
        Standing { connected: true, .. } => "Streaming",
        Standing { refused: true, .. } => "Stopped on the Mac",
        Standing { online: Some(false), .. } => "Not online",
        _ => "Connecting",
    }
    .into()
}

/// The status line where nothing else names the Mac (the tray's menu and tooltip).
pub fn status_line(mac: &str, s: Standing) -> String {
    match s {
        Standing { stopped: true, online: Some(true), .. } => format!("Stopped · {mac} is online"),
        Standing { stopped: true, online: Some(false), .. } => format!("Stopped · {mac} is not online"),
        // The first look on the network takes a moment.
        Standing { stopped: true, .. } => format!("Stopped · {mac}"),
        Standing { elsewhere: true, .. } => format!("{mac} is streaming elsewhere"),
        Standing { connected: true, enabled: false, .. } => format!("Paused · {mac}"),
        // The state first, as "Paused · …": narrow menus (Omarchy's) cut the end off.
        Standing { connected: true, locked: true, .. } => format!("Locked · {mac}"),
        Standing { connected: true, .. } => format!("Streaming from {mac}"),
        // Online, but it turns this computer away until allowed again from its own menu.
        Standing { refused: true, .. } => format!("Waiting for {mac} · stopped on the Mac"),
        // Started: it streams as soon as the Mac is back.
        Standing { online: Some(false), .. } => format!("Waiting for {mac} · not online"),
        _ => format!("Waiting for {mac}"),
    }
}

/// What can be done about the Mac in use, in order (Do actions; see `action_label`): Streaming:
/// Pause and Stop; Paused: Resume and Stop; Stopped: Start; started but waiting for the Mac: Stop.
pub fn stream_actions(s: Standing) -> &'static [&'static str] {
    match s {
        Standing { stopped: true, .. } => &["start"],
        // Start shown, but not to be chosen (see can_start); Stop ends the waiting.
        Standing { elsewhere: true, .. } => &["start", "stop"],
        Standing { connected: false, .. } => &["stop"],
        Standing { enabled: true, .. } => &["pause", "stop"],
        _ => &["resume", "stop"],
    }
}

/// Start can be chosen: not while the Mac streams to another computer (it has to be stopped
/// there first).
pub fn can_start(s: Standing) -> bool {
    !s.elsewhere
}

/// What a shortcut's toggle (Do "pause-toggle" / "stream-toggle", see `panel_do`) does now: one
/// of the actions, or None when there's nothing it can do. Pause or resume: Pause while
/// streaming, Resume while paused, nothing otherwise (stopped, waiting for the Mac). Stop or
/// start: Start when stopped (if it can be chosen), Stop otherwise (also ends the waiting).
pub fn toggle_action(toggle: &str, s: Standing) -> Option<&'static str> {
    match toggle {
        "pause-toggle" => stream_actions(s).iter().copied().find(|a| matches!(*a, "pause" | "resume")),
        "stream-toggle" if s.stopped => can_start(s).then_some("start"),
        "stream-toggle" => Some("stop"),
        _ => None,
    }
}

/// What a Start that can't be chosen says instead.
pub fn start_hint() -> String {
    "Stop streaming elsewhere first".into()
}

pub fn action_label(action: &str) -> &'static str {
    match action {
        "start" => "Start",
        "pause" => "Pause",
        "resume" => "Resume",
        _ => "Stop",
    }
}

fn standing(state: &State) -> Standing {
    let stopped = crate::net::offline();
    // Stopped here, the Mac in use still says on the network whether it's busy or locked.
    let (busy, locked) = if stopped { crate::net::in_use_says() } else { (false, false) };
    Standing {
        stopped,
        elsewhere: state.elsewhere || busy,
        connected: state.settings.is_some(),
        enabled: state.settings.as_ref().is_none_or(|s| s.enabled),
        locked: crate::net::mac_locked() || locked,
        refused: state.mac_refused,
        online: crate::net::mac_online(),
    }
}

/// The Mac in use, by the name given here (renamable) over the one it reports.
fn mac_name(state: &State, cfg: &crate::config::Config) -> String {
    cfg.macs
        .iter()
        .find(|m| m.fingerprint == cfg.host_fingerprint)
        .map(|m| m.name.clone())
        .or_else(|| state.mac.clone())
        .unwrap_or_default()
}

fn status_of(state: &State) -> String {
    let cfg = crate::config::Config::load_or_default().unwrap_or_default();
    let name = mac_name(state, &cfg);
    status_line(if name.is_empty() { "Mac" } else { &name }, standing(state))
}

fn status() -> String {
    status_of(&STATE.lock().unwrap())
}

/// The Mac's apps as the widget and Sweeflow's window list them: pinned first (in their order),
/// then running, then the rest; by name within each.
fn sort_apps(apps: &mut [serde_json::Value], pinned: &[String]) {
    let pin_pos = |a: &serde_json::Value| pinned.iter().position(|p| a["id"] == **p).unwrap_or(usize::MAX);
    apps.sort_by_key(|a| (pin_pos(a), a["state"].as_u64().unwrap_or(0) == 0, a["name"].as_str().unwrap_or_default().to_lowercase()));
}

/// Where a linked Mac is (or was last) reached: "address:port". Once it goes offline, still the
/// address it was reached at (not the name it was linked by, which would read as a change).
fn reached(m: &crate::config::PairedMac, in_use: &str) -> String {
    crate::net::connected_to()
        .filter(|_| m.fingerprint == in_use)
        .or_else(|| crate::net::last_reached(&m.fingerprint))
        .map_or_else(|| m.host.clone(), |a| a.to_string())
}

/// "address:port" without its port (and an IPv6 address without its brackets).
fn address_only(host: &str) -> String {
    host.parse::<std::net::SocketAddr>()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| host.rsplit_once(':').map_or(host, |(n, _)| n).to_string())
}

/// Everything the tray widget shows.
pub fn panel_state() -> serde_json::Value {
    let state = STATE.lock().unwrap();
    let cfg = crate::config::Config::load_or_default().unwrap_or_default();
    let kinds = load_kinds();
    let macs: Vec<_> = cfg
        .macs
        .iter()
        .map(|m| {
            serde_json::json!({ "name": m.name, "fingerprint": m.fingerprint, "active": m.fingerprint == cfg.host_fingerprint,
                                "kind": kinds.get(&m.fingerprint).map(String::as_str).unwrap_or(""),
                                // Reachable on the network: null until known.
                                "online": crate::net::online(&m.fingerprint),
                                // Streaming to another computer (as its network announcement says).
                                "busy": crate::net::busy(&m.fingerprint),
                                "locked": crate::net::locked(&m.fingerprint),
                                // How it's reached (net::route_kind) and at what address: where it
                                // is connected now for the Mac in use, else where it was last.
                                "via": crate::net::route_kind(&reached(m, &cfg.host_fingerprint)),
                                "address": address_only(&reached(m, &cfg.host_fingerprint)) })
        })
        .collect();
    let mac_name = mac_name(&state, &cfg);
    let mut apps: Vec<_> = state
        .apps
        .iter()
        .map(|(b, n)| {
            let mut a = crate::panel::app(b, n);
            a["state"] = state.running.get(b).copied().unwrap_or(0).into();
            a["pinned"] = state.pinned.contains(b).into();
            a
        })
        .collect();
    sort_apps(&mut apps, &state.pinned);
    let standing = standing(&state);
    serde_json::json!({
        "connected": state.settings.is_some(),
        "locked": crate::net::mac_locked(),
        "paired": !cfg.host.is_empty(),
        "mac": mac_name,
        "settings": state.settings.clone().unwrap_or_else(|| away_view(&state)),
        "pending": !state.pending.is_empty(),
        "apps": apps,
        "macs": macs,
        "openAtLogin": open_at_login(),
        "square": crate::config::square_corners(),
        "displayShown": DISPLAY_SHOWN.load(std::sync::atomic::Ordering::Relaxed),
        "placement": state.placement,
        "pinned": state.pinned,
        // Stop was chosen (the name is from when it was called Disconnect).
        "offline": crate::net::offline(),
        // The Mac can be reached, streaming or not: null until known.
        "online": crate::net::mac_online(),
        // The status (the reader shows the Mac's name above it), and what can be done (Do
        // actions, in order).
        "status": status_short(standing),
        "actions": stream_actions(standing),
        "macRefused": state.mac_refused,
        // The Mac is streaming to another computer (never named: nothing here knows which);
        // Start can't be chosen meanwhile ("Stop streaming elsewhere first").
        "elsewhere": state.elsewhere,
        "canStart": can_start(standing),
        "showAt": state.show_at,
        // The keys the desktop has now for open / pause / stop (and the Global menu's): as
        // shown, "" for one with none.
        "keys": crate::keys::current().json(),
        // The shortcut setup wants the tray widget to give itself (setup.rs, WIDGET_KEY).
        "setKey": crate::setup::widget_key().map(|(id, key)| serde_json::json!({ "id": id, "key": key })),
        // This copy's version, a newer release (and how it's updated), the Mac's version if it differs.
        "updates": crate::update::panel_json(),
    })
}

/// A change from the tray widget: only the fields it sent.
pub fn panel_set(json: &str) {
    let Ok(serde_json::Value::Object(changes)) = serde_json::from_str::<serde_json::Value>(json) else {
        return log::warn!("bad settings from the tray widget: {json}");
    };
    change(move |s| {
        let Ok(serde_json::Value::Object(mut now)) = serde_json::to_value(&*s) else { return };
        now.extend(changes);
        match serde_json::from_value(serde_json::Value::Object(now)) {
            Ok(new) => *s = new,
            Err(e) => log::warn!("bad settings from the tray widget: {e}"),
        }
    });
    refresh();
}

/// An action from the tray widget.
pub fn panel_do(action: &str, arg: &str) {
    match action {
        "wake" => {
            std::thread::spawn(crate::wake::wake);
        }
        "pair" => open_wizard(),
        // Check for updates, and a newer release's What's new: the update window (which updates
        // install.sh's copy itself, from a scope of its own: the service restarts under it).
        // "update" is what a widget from before the window sends.
        "update-window" | "update" => open_update_window(),
        // What's new: the release's page.
        "update-page" => {
            let _ = std::process::Command::new("xdg-open").arg(arg).spawn();
        }
        "start" if elsewhere() => log::info!("Start: the Mac is streaming to another computer"),
        "start" | "stop" => set_started(action == "start"),
        // What a widget from before Start / Stop still sends, until Plasma reloads it.
        "connect" | "disconnect" => set_started(action == "connect"),
        // With the Mac away, applied once it's back.
        "pause" | "resume" => change(|s| s.enabled = action == "resume"),
        // The shortcuts (Cmd+Opt+Y, Cmd+Opt+Z): whichever of the above fits now.
        "pause-toggle" | "stream-toggle" => {
            let state = STATE.lock().unwrap();
            let chosen = toggle_action(action, standing(&state));
            drop(state);
            match chosen {
                Some(a) => panel_do(a, ""),
                None => log::info!("{action}: nothing to do now"),
            }
        }
        "show" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            STATE.lock().unwrap().show_at = now.as_millis() as u64;
            refresh();
        }
        "switch" => switch_to(arg),
        "use" => use_mac(arg),
        "forget" => forget(arg),
        "launch" => {
            if let Err(e) = crate::launchers::launch(arg) {
                log::warn!("launch {arg}: {e:#}");
            }
        }
        "login" => {
            let verb = if arg == "on" { "enable" } else { "disable" };
            let _ = std::process::Command::new("systemctl").args(["--user", verb, "sweeflow"]).status();
            refresh();
        }
        "square" => {
            crate::config::set_square_corners(arg == "on");
            refresh();
        }
        "pin" | "unpin" => {
            let mut s = STATE.lock().unwrap();
            s.pinned.retain(|b| b != arg);
            if action == "pin" {
                s.pinned.push(arg.to_string());
            }
            save_pinned(&s.pinned);
            drop(s);
            refresh();
        }
        "placement" => refresh_placement(),
        "global-menu" => {
            let (action, arg) = (action.to_string(), arg.to_string());
            std::thread::spawn(move || {
                if let Err(e) = crate::placement::set_global_menu(arg == "on") {
                    log::warn!("{action} {arg}: {e:#}");
                }
                refresh_placement();
            });
        }
        "quit-app" => {
            if let Some(net) = NET.get() {
                let _ = net.send(NetCmd::Control(W::new(t::QUIT_APP).str(arg).finish()));
            }
        }
        "shortcut-settings" => open_shortcut_settings(),
        "linux-keys" | "shortcuts" => crate::ui::run_own("shortcuts", "Shortcuts"),
        // One of Sweeflow's keys was changed in its Shortcuts window (keys.json): the desktop
        // gets it (Hyprland's bindings, the shortcut in GNOME's or Cinnamon's settings), and the
        // menu and panels show it.
        "keys-changed" => {
            let name = arg.to_string();
            crate::hyprland::rebind_own_keys();
            std::thread::spawn(move || {
                crate::setup::apply_own_key(&name);
                refresh();
            });
        }
        // The keys kept for Linux changed (Shortcuts window): Hyprland's key mode is made
        // again with them (the other desktops read them on each key).
        "exceptions-changed" => crate::hyprland::refresh_mac_keys(),
        // The desktop's own dialog for Sweeflow's keys (the portal's: globalkeys.rs).
        "configure-keys" => {
            std::thread::spawn(|| {
                if !crate::globalkeys::configure() {
                    open_shortcut_settings();
                }
            });
        }
        // The tray widget has given itself the key setup left for it.
        "key-set" => {
            let _ = std::fs::remove_file(crate::config::dir().join(crate::setup::WIDGET_KEY));
            refresh();
        }
        "display" => {
            if let Some(net) = NET.get() {
                let show = !DISPLAY_SHOWN.load(std::sync::atomic::Ordering::Relaxed);
                let _ = net.send(NetCmd::Control(W::new(t::SHOW_DISPLAY).u8(show as u8).finish()));
            }
        }
        "quit" => quit(),
        _ => log::warn!("unknown tray widget action {action}"),
    }
}

/// Starts (which always means unpaused: never back into a pause from before), or stops: the
/// Mac's windows go and its one session is left free for another computer.
fn set_started(start: bool) {
    if start {
        // With the Mac still away, kept and applied once it's back.
        change(|s| s.enabled = true);
    }
    crate::net::set_offline(!start);
    refresh();
}

/// Looks again at where Mac apps show up (off the calling thread), and tells the widget.
pub fn refresh_placement() {
    std::thread::spawn(|| {
        // Plasma applies some changes (a new panel) a moment later.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let p = crate::placement::query();
        let mut s = STATE.lock().unwrap();
        if s.placement != p {
            s.placement = p;
            drop(s);
            refresh();
        }
    });
}

/// Starts with the session: the user service is enabled.
fn open_at_login() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "sweeflow"])
        .status()
        .is_ok_and(|s| s.success())
}

/// The Mac stopped this computer from its menu, and hasn't allowed it again.
pub fn mac_refused() -> bool {
    STATE.lock().unwrap().mac_refused
}

/// The Mac is streaming to another computer; false once it streams here, or Stop is chosen.
pub fn set_elsewhere(on: bool) {
    let mut s = STATE.lock().unwrap();
    if s.elsewhere != on {
        s.elsewhere = on;
        drop(s);
        refresh();
    }
}

pub fn elsewhere() -> bool {
    STATE.lock().unwrap().elsewhere
}

/// The Mac in use, as this computer shows it.
pub fn mac_display_name() -> String {
    let cfg = crate::config::Config::load_or_default().unwrap_or_default();
    let name = mac_name(&STATE.lock().unwrap(), &cfg);
    if name.is_empty() { "Your Mac".into() } else { name }
}


/// The Mac in use forgot this computer (from its menu): forget it here too.
pub fn mac_forgot_us() {
    let Ok(cfg) = crate::config::Config::load_or_default() else { return };
    mac_unlinked_us(&cfg.host_fingerprint);
}

/// A linked Mac unlinked this computer (from its menu): unlinked here too, and said so.
pub fn mac_unlinked_us(fingerprint: &str) {
    let Ok(cfg) = crate::config::Config::load_or_default() else { return };
    let Some(mac) = cfg.macs.iter().find(|m| m.fingerprint == fingerprint) else { return };
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=Sweeflow", "--icon=sweeflow", &format!("{} unlinked this computer", mac.name),
               "It was unlinked on the Mac, so it's unlinked here too. Link it again with Link a Mac…"])
        .spawn();
    forget_here(fingerprint);
}

/// What sort of Mac each linked one is ("laptop", "imac", "desktop"), by fingerprint: its icon.
/// Kept beside the config, so it shows while the Mac is away.
fn kinds_file() -> std::path::PathBuf {
    crate::config::dir().join("mac-kinds.json")
}

fn load_kinds() -> std::collections::HashMap<String, String> {
    serde_json::from_slice(&std::fs::read(kinds_file()).unwrap_or_default()).unwrap_or_default()
}

/// The connected Mac said what sort it is.
pub fn set_mac_kind(kind: &str) {
    let Ok(cfg) = crate::config::Config::load_or_default() else { return };
    let mut kinds = load_kinds();
    if kinds.get(&cfg.host_fingerprint).map(String::as_str) != Some(kind) {
        kinds.insert(cfg.host_fingerprint.clone(), kind.to_string());
        if let Ok(json) = serde_json::to_vec(&kinds) {
            let _ = std::fs::write(kinds_file(), json);
        }
        refresh();
    }
}

pub fn rename(fingerprint: &str, name: &str) {
    let name = name.trim();
    let Ok(mut cfg) = crate::config::Config::load_or_default() else { return };
    let Some(m) = cfg.macs.iter_mut().find(|m| m.fingerprint == fingerprint) else { return };
    if name.is_empty() || m.name == name {
        return;
    }
    m.name = name.to_string();
    if let Err(e) = cfg.save() {
        return log::warn!("saving config: {e:#}");
    }
    refresh();
}

/// The update window: looks for a newer release, shows what's new and updates.
pub fn open_update_window() {
    run_window("update-window", "the update window");
    // The window looks for itself (it's a process of its own); the panel's line and the
    // notification come from this one's look.
    std::thread::spawn(|| {
        if let Err(e) = crate::update::check() {
            log::info!("update check: {e:#}");
        }
    });
}

pub fn open_wizard() {
    run_window("pair", "the pairing window");
}

/// Sweeflow's own window (`sweeflow panel`), where the Plasma widget isn't. One at a time: a
/// second one exits at once.
fn open_panel() {
    run_window("panel", "Sweeflow's window");
}

/// Opens one of Sweeflow's own windows: this binary again, with `arg`.
///
/// Run by the service, the window goes into a scope of its own (systemd-run --scope): left in
/// the service's group, the service restarting (as it does to stream from a Mac just linked)
/// took it down too, and "Link a Mac" vanished before its last pages.
fn run_window(arg: &str, what: &str) {
    let me = match std::env::current_exe() {
        Ok(me) => me,
        Err(e) => return log::warn!("couldn't open {what}: {e}"),
    };
    let scoped = std::env::var_os("INVOCATION_ID").is_some().then(|| {
        let mut c = std::process::Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect", "--"]).arg(&me).arg(arg);
        c
    });
    let child = scoped.and_then(|mut c| c.spawn().inspect_err(|e| log::info!("systemd-run: {e}")).ok());
    let child = match child {
        Some(c) => Ok(c),
        None => std::process::Command::new(&me).arg(arg).spawn(),
    };
    match child {
        // Waited for, so it doesn't stay behind as a dead process once its window closes.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("couldn't open {what}: {e}"),
    }
}

/// Streams from another paired Mac: saved, then the client restarts into it (which drops every
/// window of the old one cleanly).
fn switch_to(fingerprint: &str) {
    let mut cfg = match crate::config::Config::load_or_default() {
        Ok(c) => c,
        Err(e) => return log::warn!("config: {e:#}"),
    };
    if cfg.host_fingerprint == fingerprint || !cfg.activate(fingerprint) {
        return;
    }
    if let Err(e) = cfg.save() {
        return log::warn!("saving config: {e:#}");
    }
    log::info!("switching to {}", cfg.host);
    crate::net::set_offline(false);
    restart();
}

/// A Mac just linked (the Link a Mac window says): streams from it now, switching from the one in
/// use if there is one, and starting if stopped.
fn use_mac(fingerprint: &str) {
    let mut cfg = match crate::config::Config::load_or_default() {
        Ok(c) => c,
        Err(e) => return log::warn!("config: {e:#}"),
    };
    if !cfg.activate(fingerprint) {
        return log::warn!("no linked Mac {fingerprint}");
    }
    if let Err(e) = cfg.save() {
        return log::warn!("saving config: {e:#}");
    }
    log::info!("streaming from the Mac just linked, {}", cfg.host);
    crate::net::set_offline(false);
    // A Mac already streaming to another computer answers Busy: "In use by …" until it's free.
    restart();
}

/// Unlink, chosen here: the Mac is told too (now, or once it's seen again), and forgets this
/// computer.
fn forget(fingerprint: &str) {
    let Ok(cfg) = crate::config::Config::load_or_default() else { return };
    if let Some(mac) = cfg.macs.iter().find(|m| m.fingerprint == fingerprint) {
        crate::pair::tell_unlinked(mac.clone());
    }
    forget_here(fingerprint);
}

/// Forgets a Mac on this side only.
fn forget_here(fingerprint: &str) {
    let Ok(mut cfg) = crate::config::Config::load_or_default() else { return };
    let was_active = cfg.host_fingerprint == fingerprint;
    cfg.forget(fingerprint);
    if let Err(e) = cfg.save() {
        return log::warn!("saving config: {e:#}");
    }
    refresh();
    if was_active {
        // Unlinking the Mac in use disconnects; it doesn't go on to another linked Mac (one
        // locked or far away showed its windows as paused stills). Another one, now listed
        // first, is there to Start.
        crate::net::set_offline(true);
        restart();
    }
}

fn restart() {
    // Switching Macs or forgetting the one in use: the old Mac is told, and tidies up at once.
    crate::net::goodbye();
    // Through systemd (it runs us); a plain exit would also do, but slower (RestartSec).
    let _ = std::process::Command::new("systemctl").args(["--user", "restart", "sweeflow"]).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_as_the_mac_sends_them() {
        let json = br#"{"enabled":true,"exclude":["com.apple.finder"],"sound":false,"dim":true,"clipboard":true,"dragDrop":false,"launcher":true,"quality":"light","desktop":false}"#;
        let s: Settings = serde_json::from_slice(json).unwrap();
        assert!(s.enabled && !s.sound && !s.drag_drop && !s.desktop);
        assert_eq!(s.exclude, ["com.apple.finder"]);
        assert_eq!(s.quality, "light");
        // And back, in the Mac's field names.
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains("\"dragDrop\":false"));
    }

    #[test]
    fn an_older_mac_without_the_desktop_setting_means_on() {
        let json = br#"{"enabled":true,"exclude":[],"sound":true,"dim":true,"clipboard":true,"dragDrop":true,"launcher":true,"quality":"sharp"}"#;
        let s: Settings = serde_json::from_slice(json).unwrap();
        assert!(s.desktop);
    }

    #[test]
    fn the_status_line_for_each_state() {
        let streaming = Standing { connected: true, enabled: true, online: Some(true), ..Default::default() };
        assert_eq!(status_line("Studio", streaming), "Streaming from Studio");
        assert_eq!(status_short(streaming), "Streaming");
        assert_eq!(status_short(Standing { enabled: false, ..streaming }), "Paused");
        assert_eq!(status_line("Studio", Standing { locked: true, ..streaming }), "Locked · Studio");
        assert_eq!(status_line("Studio", Standing { enabled: false, ..streaming }), "Paused · Studio");
        // Locked while paused: the pause is what matters here.
        assert_eq!(status_line("Studio", Standing { enabled: false, locked: true, ..streaming }), "Paused · Studio");

        let stopped = Standing { stopped: true, ..Default::default() };
        assert_eq!(status_line("Studio", Standing { online: Some(true), ..stopped }), "Stopped · Studio is online");
        assert_eq!(status_line("Studio", Standing { online: Some(false), ..stopped }), "Stopped · Studio is not online");
        assert_eq!(status_line("Studio", stopped), "Stopped · Studio");
        let elsewhere = Standing { elsewhere: true, online: Some(true), ..Default::default() };
        assert_eq!(status_line("Studio", elsewhere), "Studio is streaming elsewhere");
        assert_eq!(status_short(elsewhere), "Streaming elsewhere");

        let waiting = Standing { enabled: true, online: Some(false), ..Default::default() };
        assert_eq!(status_line("Studio", waiting), "Waiting for Studio · not online");
        assert_eq!(status_line("Studio", Standing { online: None, ..waiting }), "Waiting for Studio");
        assert_eq!(status_line("Studio", Standing { refused: true, online: Some(true), ..waiting }), "Waiting for Studio · stopped on the Mac");
    }

    #[test]
    fn each_state_has_its_actions_and_no_others() {
        let streaming = Standing { connected: true, enabled: true, online: Some(true), ..Default::default() };
        assert_eq!(stream_actions(streaming), ["pause", "stop"]);
        assert_eq!(stream_actions(Standing { locked: true, ..streaming }), ["pause", "stop"]);
        assert_eq!(stream_actions(Standing { enabled: false, ..streaming }), ["resume", "stop"]);
        assert_eq!(stream_actions(Standing { stopped: true, online: Some(false), ..Default::default() }), ["start"]);
        // In use by another computer: Start shown but not to be chosen; Stop ends the waiting.
        let elsewhere = Standing { elsewhere: true, online: Some(true), ..Default::default() };
        assert_eq!(stream_actions(elsewhere), ["start", "stop"]);
        assert!(!can_start(elsewhere));
        assert!(can_start(Standing { stopped: true, ..Default::default() }));
        assert_eq!(start_hint(), "Stop streaming elsewhere first");
        assert_eq!(stream_actions(Standing { online: Some(false), ..Default::default() }), ["stop"]);
        let labels: Vec<&str> = ["start", "pause", "resume", "stop"].into_iter().map(action_label).collect();
        assert_eq!(labels, ["Start", "Pause", "Resume", "Stop"]);
    }

    #[test]
    fn the_shortcuts_toggle_what_fits_now() {
        let streaming = Standing { connected: true, enabled: true, online: Some(true), ..Default::default() };
        let paused = Standing { enabled: false, ..streaming };
        let stopped = Standing { stopped: true, online: Some(true), ..Default::default() };
        let waiting = Standing { online: Some(false), ..Default::default() };
        let busy = Standing { stopped: true, elsewhere: true, ..Default::default() };
        let started_elsewhere = Standing { elsewhere: true, online: Some(true), ..Default::default() };
        assert_eq!(toggle_action("pause-toggle", streaming), Some("pause"));
        assert_eq!(toggle_action("pause-toggle", Standing { locked: true, ..streaming }), Some("pause"));
        assert_eq!(toggle_action("pause-toggle", paused), Some("resume"));
        assert_eq!(toggle_action("pause-toggle", stopped), None);
        assert_eq!(toggle_action("pause-toggle", waiting), None);
        assert_eq!(toggle_action("stream-toggle", streaming), Some("stop"));
        assert_eq!(toggle_action("stream-toggle", paused), Some("stop"));
        assert_eq!(toggle_action("stream-toggle", waiting), Some("stop"));
        assert_eq!(toggle_action("stream-toggle", started_elsewhere), Some("stop"));
        assert_eq!(toggle_action("stream-toggle", stopped), Some("start"));
        // The Mac streams to another computer: it has to be stopped there first.
        assert_eq!(toggle_action("stream-toggle", busy), None);
        assert_eq!(toggle_action("show", streaming), None);
    }

    #[test]
    fn apps_list_pinned_then_running_then_the_rest() {
        let app = |id: &str, name: &str, state: u8| serde_json::json!({ "id": id, "name": name, "state": state });
        let mut apps = vec![
            app("z", "zed", 0),
            app("s", "Safari", 3),
            app("n", "Notes", 0),
            app("m", "Mail", 1),
            app("f", "Finder", 0),
            app("t", "Terminal", 2),
        ];
        sort_apps(&mut apps, &["t".into(), "f".into()]);
        let names: Vec<&str> = apps.iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Terminal", "Finder", "Mail", "Safari", "Notes", "zed"]);
    }
}
