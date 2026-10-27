//! The tray icon: streaming on and off, which Mac apps, and the features, with the same switches
//! as the Mac's menu-bar item. The Mac keeps the settings; changing them on either side changes
//! them on both.

use std::sync::{LazyLock, Mutex, OnceLock};

use ksni::{
    blocking::{Handle, TrayMethods},
    menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu},
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
    /// Behind Mac windows on the Mac's virtual display: "auto" (as the Mac's appearance),
    /// "dark" or "light". Older Macs don't send it.
    #[serde(default = "auto")]
    pub backdrop: String,
}

fn auto() -> String {
    "auto".into()
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
            dim: true,
            clipboard: true,
            drag_drop: true,
            launcher: true,
            quality: "sharp".into(),
            desktop: true,
            backdrop: auto(),
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
    /// Where Mac apps show up (task manager, Mac Dock, Global Menu), as last looked at.
    placement: crate::placement::Placement,
    /// When Sweeflow was last opened from the app menu (ms since the epoch): the widget pops up.
    show_at: u64,
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

pub fn start(net: UnboundedSender<NetCmd>) {
    let _ = NET.set(net);
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = REFRESH.set(tx);
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let panel = crate::panel::Panel::new().map_err(|e| log::warn!("tray widget's D-Bus service: {e}")).ok();
            refresh_placement();
            // With the KDE widget installed, it is the tray icon; the plain one is for elsewhere.
            let handle: Option<Handle<Tray>> = if widget_installed() {
                None
            } else {
                match Tray.spawn() {
                    Ok(h) => Some(h),
                    Err(e) => {
                        log::warn!("tray icon: {e}");
                        None
                    }
                }
            };
            while rx.recv().is_ok() {
                // Several changes at once: one redraw.
                while rx.try_recv().is_ok() {}
                if let Some(h) = &handle {
                    h.update(|_| {});
                }
                if let Some(p) = &panel {
                    p.changed();
                }
            }
        })
        .unwrap();
}

fn refresh() {
    if let Some(tx) = REFRESH.get() {
        let _ = tx.send(());
    }
}

pub fn connected(mac: Option<String>) {
    let mut s = STATE.lock().unwrap();
    if mac.is_none() {
        s.settings = None;
        s.running.clear();
    }
    s.mac = mac;
    drop(s);
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
            // Not tied to streaming being on: pausing would take the desktop away (and back
            // under a new id), and windows opening meanwhile landed on the shared desktop.
            let wanted = new.desktop;
            // Acted on when it changes, and once per run (in case it changed while away).
            static APPLIED: Mutex<Option<bool>> = Mutex::new(None);
            if APPLIED.lock().unwrap().replace(wanted) != Some(wanted) {
                crate::desktop::apply(wanted);
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

pub fn allows_clipboard() -> bool {
    get(|s| s.enabled && s.clipboard, true)
}

pub fn wants_own_desktop() -> bool {
    get(|s| s.desktop, false)
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
        drop(state);
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

struct Tray;

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "sweeflow".into()
    }

    fn title(&self) -> String {
        "sweeflow".into()
    }

    fn icon_name(&self) -> String {
        "computer-laptop-symbolic".into()
    }

    fn overlay_icon_name(&self) -> String {
        let s = STATE.lock().unwrap();
        match &s.settings {
            None => "network-disconnect".into(),
            Some(st) if !st.enabled => "media-playback-paused".into(),
            _ => String::new(),
        }
    }

    fn tool_tip(&self) -> ToolTip {
        let s = STATE.lock().unwrap();
        let description = match (&s.mac, &s.settings) {
            (Some(mac), Some(st)) if st.enabled => format!("Streaming apps from {mac}"),
            (Some(mac), _) => format!("Connected to {mac}, streaming off"),
            (None, _) if crate::net::offline() => "Disconnected".into(),
            (None, _) => "Mac not connected".into(),
        };
        ToolTip { title: "sweeflow".into(), description, ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = STATE.lock().unwrap();
        let known = state.settings.is_some();
        let s = Some(state.settings.clone().unwrap_or_else(|| away_view(&state)));
        let on = s.as_ref().is_some_and(|s| s.enabled);
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: match &state.mac {
                    Some(m) => format!("Connected to {m}"),
                    None if crate::net::offline() => "Disconnected".into(),
                    None => "Mac not connected".into(),
                },
                enabled: false,
                ..Default::default()
            }
            .into(),
            // With the Mac away, applied once it's back.
            CheckmarkItem {
                label: "Stream Mac apps".into(),
                checked: on,
                activate: Box::new(|_| change(|s| s.enabled = !s.enabled)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Wake Mac".into(),
                icon_name: "system-reboot".into(),
                visible: !known && !crate::net::offline(),
                activate: Box::new(|_| {
                    std::thread::spawn(crate::wake::wake);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if crate::net::offline() { "Connect" } else { "Disconnect" }.into(),
                icon_name: if crate::net::offline() { "network-connect" } else { "network-disconnect" }.into(),
                activate: Box::new(|_| set_connected(crate::net::offline())),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];

        items.push(macs_menu());
        items.push(MenuItem::Separator);

        let exclude = s.as_ref().map(|s| s.exclude.clone()).unwrap_or_default();
        let apps: Vec<MenuItem<Self>> = state
            .apps
            .iter()
            .map(|(bid, name)| {
                let bid = bid.clone();
                CheckmarkItem {
                    // Underscores would be taken as keyboard accelerators.
                    label: name.replace('_', "__"),
                    checked: !exclude.contains(&bid),
                    activate: Box::new(move |_| {
                        let bid = bid.clone();
                        change(move |s| {
                            if let Some(i) = s.exclude.iter().position(|b| *b == bid) {
                                s.exclude.remove(i);
                            } else {
                                s.exclude.push(bid);
                            }
                        })
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        items.push(SubMenu { label: "Apps".into(), enabled: !apps.is_empty(), submenu: apps, ..Default::default() }.into());
        items.push(MenuItem::Separator);

        type Toggle = (&'static str, fn(&Settings) -> bool, fn(&mut Settings));
        let toggles: [Toggle; 6] = [
            ("Mac sound on Linux", |s| s.sound, |s| s.sound = !s.sound),
            ("Dim Mac screen", |s| s.dim, |s| s.dim = !s.dim),
            ("Shared clipboard", |s| s.clipboard, |s| s.clipboard = !s.clipboard),
            ("Drag and drop", |s| s.drag_drop, |s| s.drag_drop = !s.drag_drop),
            ("Mac Apps in app menu", |s| s.launcher, |s| s.launcher = !s.launcher),
            ("Mac apps on their own desktop", |s| s.desktop, |s| s.desktop = !s.desktop),
        ];
        for (label, get, flip) in toggles {
            items.push(
                CheckmarkItem {
                    label: label.into(),
                    checked: s.as_ref().is_some_and(get),
                    activate: Box::new(move |_| change(flip)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            SubMenu {
                label: "Quality".into(),
                submenu: vec![
                    RadioGroup {
                        selected: usize::from(s.as_ref().is_some_and(|s| s.quality == "light")),
                        select: Box::new(|_, i| change(|s| s.quality = if i == 1 { "light" } else { "sharp" }.into())),
                        options: vec![
                            RadioItem { label: "Sharpest".into(), ..Default::default() },
                            RadioItem { label: "Lighter (for slower networks)".into(), ..Default::default() },
                        ],
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit sweeflow".into(),
                icon_name: "application-exit".into(),
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

/// "Mac ▸": which paired Mac to stream from, adding one, forgetting one.
fn macs_menu() -> MenuItem<Tray> {
    let cfg = crate::config::Config::load_or_default().unwrap_or_default();
    let mut sub: Vec<MenuItem<Tray>> = Vec::new();
    if !cfg.macs.is_empty() {
        let fps: Vec<String> = cfg.macs.iter().map(|m| m.fingerprint.clone()).collect();
        sub.push(
            RadioGroup {
                selected: fps.iter().position(|f| *f == cfg.host_fingerprint).unwrap_or(0),
                select: Box::new(move |_, i| {
                    let Some(fp) = fps.get(i).cloned() else { return };
                    switch_to(&fp);
                }),
                options: cfg.macs.iter().map(|m| RadioItem { label: m.name.replace('_', "__"), ..Default::default() }).collect(),
            }
            .into(),
        );
        sub.push(MenuItem::Separator);
    }
    sub.push(
        StandardItem {
            label: "Add a Mac…".into(),
            icon_name: "list-add".into(),
            activate: Box::new(|_| open_wizard()),
            ..Default::default()
        }
        .into(),
    );
    if !cfg.macs.is_empty() {
        let forget: Vec<MenuItem<Tray>> = cfg
            .macs
            .iter()
            .map(|m| {
                let fp = m.fingerprint.clone();
                StandardItem {
                    label: m.name.replace('_', "__"),
                    activate: Box::new(move |_| forget(&fp)),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        sub.push(SubMenu { label: "Forget".into(), submenu: forget, ..Default::default() }.into());
    }
    SubMenu { label: "Mac".into(), icon_name: "computer-laptop".into(), submenu: sub, ..Default::default() }.into()
}

fn quit() {
    // Through systemd, which would otherwise start it again (under the service, this stop is what
    // ends us). Started any other way (KRunner, a terminal), the stop is a no-op, so exit ourselves.
    let _ = std::process::Command::new("systemctl").args(["--user", "stop", "sweeflow"]).status();
    std::process::exit(0);
}

/// The KDE tray widget (linux/plasmoid), installed by `sweeflow install` on Plasma.
pub fn widget_installed() -> bool {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .is_some_and(|h| h.join(".local/share/plasma/plasmoids/dev.sweeflow.panel/metadata.json").exists())
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

/// Everything the tray widget shows.
pub fn panel_state() -> serde_json::Value {
    let state = STATE.lock().unwrap();
    let cfg = crate::config::Config::load_or_default().unwrap_or_default();
    let macs: Vec<_> = cfg
        .macs
        .iter()
        .map(|m| serde_json::json!({ "name": m.name, "fingerprint": m.fingerprint, "active": m.fingerprint == cfg.host_fingerprint }))
        .collect();
    // The name given here (renamable) over the one the Mac reports.
    let mac_name = cfg
        .macs
        .iter()
        .find(|m| m.fingerprint == cfg.host_fingerprint)
        .map(|m| m.name.clone())
        .or_else(|| state.mac.clone())
        .unwrap_or_default();
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
    // Pinned (in their order), then running, then the rest; by name within each.
    let pin_pos = |a: &serde_json::Value| state.pinned.iter().position(|p| a["id"] == **p).unwrap_or(usize::MAX);
    apps.sort_by_key(|a| (pin_pos(a), a["state"].as_u64() == Some(0), a["name"].as_str().unwrap_or_default().to_lowercase()));
    serde_json::json!({
        "connected": state.settings.is_some(),
        "paired": !cfg.host.is_empty(),
        "mac": mac_name,
        "settings": state.settings.clone().unwrap_or_else(|| away_view(&state)),
        "pending": !state.pending.is_empty(),
        "apps": apps,
        "macs": macs,
        "openAtLogin": open_at_login(),
        "placement": state.placement,
        "pinned": state.pinned,
        "offline": crate::net::offline(),
        "showAt": state.show_at,
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
        "disconnect" | "connect" => set_connected(action == "connect"),
        "show" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            STATE.lock().unwrap().show_at = now.as_millis() as u64;
            refresh();
        }
        "switch" => switch_to(arg),
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
        "gather" => crate::desktop::gather(),
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
        "taskbar" | "dock" | "global-menu" => {
            let (action, arg) = (action.to_string(), arg.to_string());
            std::thread::spawn(move || {
                let r = match (action.as_str(), arg.as_str()) {
                    ("taskbar", a) => crate::placement::set_taskbar(a == "on"),
                    ("dock", "off") => crate::placement::remove_dock(),
                    ("dock", _) => crate::placement::add_dock(),
                    (_, a) => crate::placement::set_global_menu(a == "on"),
                };
                if let Err(e) = r {
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
        "display" => {
            if let Some(net) = NET.get() {
                let _ = net.send(NetCmd::Control(W::new(t::SHOW_DISPLAY).u8(1).finish()));
            }
        }
        "quit" => quit(),
        _ => log::warn!("unknown tray widget action {action}"),
    }
}

/// Connects (which always means streaming: never back into a pause from before) or disconnects.
fn set_connected(connect: bool) {
    if connect {
        // With the Mac still away, kept and applied once it's back.
        change(|s| s.enabled = true);
    }
    crate::net::set_offline(!connect);
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

pub fn open_wizard() {
    match std::env::current_exe() {
        Ok(me) => {
            if let Err(e) = std::process::Command::new(me).arg("pair").spawn() {
                log::warn!("couldn't open the pairing window: {e}");
            }
        }
        Err(e) => log::warn!("couldn't open the pairing window: {e}"),
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

fn forget(fingerprint: &str) {
    let Ok(mut cfg) = crate::config::Config::load_or_default() else { return };
    let was_active = cfg.host_fingerprint == fingerprint;
    cfg.forget(fingerprint);
    if let Err(e) = cfg.save() {
        return log::warn!("saving config: {e:#}");
    }
    refresh();
    if was_active {
        restart();
    }
}

fn restart() {
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
}
