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
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);
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
            let handle: Handle<Tray> = match Tray.spawn() {
                Ok(h) => h,
                Err(e) => return log::warn!("tray icon: {e}"),
            };
            while rx.recv().is_ok() {
                // Several changes at once: one redraw.
                while rx.try_recv().is_ok() {}
                handle.update(|_| {});
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
            let wanted = new.enabled && new.desktop;
            // Acted on when it changes, and once per run (in case it changed while away).
            static APPLIED: Mutex<Option<bool>> = Mutex::new(None);
            if APPLIED.lock().unwrap().replace(wanted) != Some(wanted) {
                crate::desktop::apply(wanted);
            }
            STATE.lock().unwrap().settings = Some(new);
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
            CheckmarkItem {
                label: "Stream Mac apps".into(),
                enabled: known,
                checked: on,
                activate: Box::new(|_| change(|s| s.enabled = !s.enabled)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];

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
                    // Through systemd, which would otherwise start it again.
                    let _ = std::process::Command::new("systemctl").args(["--user", "stop", "sweeflow"]).spawn();
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}
