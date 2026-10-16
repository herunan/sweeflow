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
}

fn yes() -> bool {
    true
}

#[derive(Default)]
struct State {
    /// The Mac's name while connected.
    mac: Option<String>,
    /// Known while connected.
    settings: Option<Settings>,
    /// (bundle id, name) of the Mac's apps.
    apps: Vec<(String, String)>,
    /// "Stream Mac apps" was ticked while the Mac was unreachable: switch it on when it's back.
    resume_wanted: bool,
    /// The settings as last seen from the Mac, shown (read-only) while it's away.
    last: Option<Settings>,
}

static STATE: LazyLock<Mutex<State>> =
    LazyLock::new(|| Mutex::new(State { last: load_last(), ..Default::default() }));
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
    }
    s.mac = mac;
    drop(s);
    refresh();
}

pub fn set_settings(json: &[u8]) {
    match serde_json::from_slice::<Settings>(json) {
        Ok(new) => {
            log::info!("settings: {new:?}");
            // Not tied to streaming being on: pausing would take the desktop away (and back
            // under a new id), and windows opening meanwhile landed on the shared desktop.
            let wanted = new.desktop;
            // Acted on when it changes, and once per run (in case it changed while away).
            static APPLIED: Mutex<Option<bool>> = Mutex::new(None);
            if APPLIED.lock().unwrap().replace(wanted) != Some(wanted) {
                crate::desktop::apply(wanted);
            }
            let resume = {
                let mut s = STATE.lock().unwrap();
                let resume = std::mem::take(&mut s.resume_wanted) && !new.enabled;
                save_last(&new);
                s.last = Some(new.clone());
                s.settings = Some(new);
                resume
            };
            if resume {
                log::info!("the Mac is back: switching streaming on, as asked");
                change(|s| s.enabled = true);
            }
            refresh();
            crate::net::launcher_setting_changed();
        }
        Err(e) => log::warn!("bad settings from the Mac: {e}"),
    }
}

pub fn set_apps(apps: Vec<(String, String)>) {
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
/// settings for both sides).
fn change(f: impl FnOnce(&mut Settings)) {
    let mut state = STATE.lock().unwrap();
    let Some(s) = state.settings.as_mut() else { return };
    f(s);
    let json = serde_json::to_vec(s).unwrap_or_default();
    drop(state);
    if let Some(net) = NET.get() {
        let _ = net.send(NetCmd::Control(W::new(t::SET_SETTINGS).bytes(&json).finish()));
    }
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
            (None, _) => "Mac not connected".into(),
        };
        ToolTip { title: "sweeflow".into(), description, ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = STATE.lock().unwrap();
        let s = state.settings.clone();
        let known = s.is_some();
        let on = s.as_ref().is_some_and(|s| s.enabled);
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: state.mac.as_ref().map_or("Mac not connected".into(), |m| format!("Connected to {m}")),
                enabled: false,
                ..Default::default()
            }
            .into(),
            if known {
                CheckmarkItem {
                    label: "Stream Mac apps".into(),
                    checked: on,
                    activate: Box::new(|_| change(|s| s.enabled = !s.enabled)),
                    ..Default::default()
                }
                .into()
            } else {
                // The Mac can't be reached (asleep, most likely): remember the wish, try to wake
                // it, and switch streaming on once it's back.
                CheckmarkItem {
                    label: "Stream Mac apps (when the Mac is back)".into(),
                    checked: state.resume_wanted,
                    activate: Box::new(|_| {
                        let mut s = STATE.lock().unwrap();
                        s.resume_wanted = !s.resume_wanted;
                        let wanted = s.resume_wanted;
                        drop(s);
                        if wanted {
                            std::thread::spawn(crate::wake::wake);
                        }
                    }),
                    ..Default::default()
                }
                .into()
            },
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
        items.push(SubMenu { label: "Apps".into(), enabled: on && !apps.is_empty(), submenu: apps, ..Default::default() }.into());
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
                    enabled: on,
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
                enabled: on,
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
    let mut apps: Vec<_> = state.apps.iter().map(|(b, n)| crate::panel::app(b, n)).collect();
    apps.sort_by_key(|a| a["name"].as_str().unwrap_or_default().to_lowercase());
    serde_json::json!({
        "connected": state.settings.is_some(),
        "paired": !cfg.host.is_empty(),
        "mac": mac_name,
        "settings": state.settings.as_ref().or(state.last.as_ref()),
        "resumeWanted": state.resume_wanted,
        "apps": apps,
        "macs": macs,
        "openAtLogin": open_at_login(),
    })
}

/// A change from the tray widget: only the fields it sent.
pub fn panel_set(json: &str) {
    let Ok(serde_json::Value::Object(changes)) = serde_json::from_str::<serde_json::Value>(json) else {
        return log::warn!("bad settings from the tray widget: {json}");
    };
    if STATE.lock().unwrap().settings.is_none() {
        // The Mac is away: only streaming on/off can be asked for, and is done when it's back.
        if let Some(on) = changes.get("enabled").and_then(|v| v.as_bool()) {
            STATE.lock().unwrap().resume_wanted = on;
            if on {
                std::thread::spawn(crate::wake::wake);
            }
            refresh();
        }
        return;
    }
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
        "display" => {
            if let Some(net) = NET.get() {
                let _ = net.send(NetCmd::Control(W::new(t::SHOW_DISPLAY).u8(1).finish()));
            }
        }
        "quit" => quit(),
        _ => log::warn!("unknown tray widget action {action}"),
    }
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
