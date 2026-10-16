//! "Add a Mac": the pairing wizard (`sweeflow pair`). Its own process, so its event loop never
//! shares a thread with the streaming client's Wayland one.
//!
//! Install Sweeflow on the Mac → pick the Mac (found on the network) → type the code it shows →
//! wait while someone clicks Allow there → done.

use std::{cell::RefCell, net::ToSocketAddrs, rc::Rc};

use anyhow::Result;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::{
    config::Config,
    pair::{self, Found, FoundMac, Outcome},
};

slint::slint! {
    import { Button, LineEdit, Spinner, VerticalBox, HorizontalBox, ScrollView } from "std-widgets.slint";

    export struct MacRow { name: string, detail: string }

    component Title inherits Text {
        font-size: 20px;
        font-weight: 600;
        wrap: word-wrap;
    }

    component Body inherits Text {
        font-size: 13px;
        wrap: word-wrap;
        color: #888;
    }

    component Row inherits Rectangle {
        in property <string> name;
        in property <string> detail;
        callback clicked;
        height: 52px;
        border-radius: 8px;
        background: touch.has-hover ? #8882 : #8881;
        touch := TouchArea { clicked => { root.clicked(); } mouse-cursor: pointer; }
        HorizontalLayout {
            padding-left: 14px; padding-right: 14px; spacing: 10px;
            VerticalLayout {
                alignment: center;
                Text { text: root.name; font-size: 14px; font-weight: 600; }
                Text { text: root.detail; font-size: 11px; color: #888; }
            }
            Rectangle {}
            Text { text: "›"; font-size: 22px; vertical-alignment: center; color: #888; }
        }
    }

    export component Wizard inherits Window {
        title: "Add a Mac — Sweeflow";
        icon: @image-url("../../assets/sweeflow.svg");
        // A fixed size: a dialog, which tiling window managers leave floating.
        width: 540px;
        height: 460px;

        in-out property <int> page: 0;
        in property <string> installer-url;
        in property <[MacRow]> macs;
        in property <string> mac-name;
        in property <string> our-code;
        in-out property <string> error;
        in property <bool> streaming-mac-set;

        callback pick(int);
        callback manual(string);
        callback submit(string);
        callback cancel-wait();
        callback finish(bool);

        VerticalBox {
            padding: 24px;
            spacing: 14px;

            if page == 0: VerticalLayout {
                spacing: 14px;
                HorizontalLayout {
                    spacing: 14px;
                    alignment: start;
                    Image { source: @image-url("../../assets/sweeflow.svg"); width: 56px; height: 56px; }
                    Title { text: "Add a Mac"; vertical-alignment: center; }
                }
                Body {
                    text: installer-url != ""
                        ? "1.  On your Mac, open this address in Safari and install Sweeflow from it:"
                        : "1.  Install Sweeflow on your Mac (drag it from Sweeflow.dmg into Applications).";
                }
                if installer-url != "": Text {
                    text: installer-url;
                    font-size: 18px;
                    font-family: "monospace";
                    horizontal-alignment: center;
                }
                if installer-url != "": Body { text: "    Page won't load? This computer's firewall may be blocking it (port 44130)."; }
                Body { text: "    The first time, macOS asks whether to open it: choose System Settings → Privacy & Security → Open Anyway."; }
                Body { text: "2.  Open Sweeflow and allow what it asks for. It then shows a code for this computer."; }
                Rectangle {}
                HorizontalLayout {
                    alignment: end;
                    Button { text: "It's installed — next"; primary: true; clicked => { page = 1; } }
                }
            }

            if page == 1: VerticalLayout {
                spacing: 12px;
                Title { text: "Choose your Mac"; }
                if macs.length == 0: HorizontalLayout {
                    spacing: 10px;
                    alignment: start;
                    Spinner { indeterminate: true; width: 20px; height: 20px; }
                    Body { text: "Looking for Macs showing a pairing code…"; vertical-alignment: center; }
                }
                ScrollView {
                    vertical-stretch: 1;
                    VerticalLayout {
                        spacing: 6px;
                        for m[i] in macs: Row { name: m.name; detail: m.detail; clicked => { root.pick(i); } }
                    }
                }
                Body { text: "Don't see it? On the Mac, choose Sweeflow → Pair with Linux… in the menu bar. Both computers need to be on the same network (a VPN can hide them from each other)."; }
                HorizontalLayout {
                    spacing: 8px;
                    addr := LineEdit { placeholder-text: "Or type the Mac's address"; accepted => { root.manual(self.text); } }
                    Button { text: "Connect"; enabled: addr.text != ""; clicked => { root.manual(addr.text); } }
                }
                if error != "": Text { text: error; color: #d33; wrap: word-wrap; }
            }

            if page == 2: VerticalLayout {
                spacing: 14px;
                Title { text: "Type the code shown on " + mac-name; }
                code := LineEdit {
                    placeholder-text: "XXXX-XXXX";
                    font-size: 22px;
                    horizontal-alignment: center;
                    accepted => { root.submit(self.text); }
                    init => { self.focus(); }
                }
                if error != "": Text { text: error; color: #d33; wrap: word-wrap; }
                Body { text: mac-name + " will then ask whether to allow this computer. It shows this computer's code, " + our-code + ": check it matches."; }
                Rectangle {}
                HorizontalLayout {
                    spacing: 8px;
                    Button { text: "Back"; clicked => { error = ""; page = 1; } }
                    Rectangle {}
                    Button { text: "Pair"; primary: true; enabled: code.text != ""; clicked => { root.submit(code.text); } }
                }
            }

            if page == 3: VerticalLayout {
                spacing: 16px;
                alignment: center;
                Spinner { indeterminate: true; width: 40px; height: 40px; }
                Title { text: "Click Allow on " + mac-name; horizontal-alignment: center; }
                Body { text: "It asks whether to allow this computer, with the code " + our-code + "."; horizontal-alignment: center; }
                HorizontalLayout { alignment: center; Button { text: "Cancel"; clicked => { root.cancel-wait(); } } }
            }

            if page == 4: VerticalLayout {
                spacing: 16px;
                HorizontalLayout {
                    spacing: 14px;
                    alignment: start;
                    Image { source: @image-url("../../assets/sweeflow.svg"); width: 56px; height: 56px; }
                    Title { text: "Paired with " + mac-name; vertical-alignment: center; }
                }
                Body {
                    text: streaming-mac-set
                        ? "You can switch between your Macs from the Sweeflow icon in the system tray."
                        : "Its apps will show up here as windows of their own.";
                }
                Rectangle {}
                HorizontalLayout {
                    spacing: 8px;
                    alignment: end;
                    if streaming-mac-set: Button { text: "Keep the current Mac"; clicked => { root.finish(false); } }
                    Button { text: "Use " + mac-name + " now"; primary: true; clicked => { root.finish(true); } }
                }
            }
        }
    }
}

thread_local! {
    /// Macs in pairing mode right now (UI thread only).
    static FOUND: RefCell<Vec<FoundMac>> = const { RefCell::new(Vec::new()) };
    static CHOSEN: RefCell<Option<FoundMac>> = const { RefCell::new(None) };
    /// Bumped to forget a pairing attempt the person cancelled.
    static ATTEMPT: RefCell<u32> = const { RefCell::new(0) };
}

pub fn run() -> Result<()> {
    // Matches sweeflow.desktop (written by `sweeflow install`), which gives KDE the logo for
    // the title bar and taskbar.
    let _ = slint::set_xdg_app_id("sweeflow");
    let ui = Wizard::new()?;
    let model = Rc::new(VecModel::<MacRow>::default());
    ui.set_macs(ModelRc::from(model.clone()));
    ui.set_our_code(pair::our_code()?.into());
    ui.set_streaming_mac_set(!Config::load_or_default()?.host.is_empty());

    let _server = crate::installer::serve().inspect_err(|e| log::info!("installer not served: {e:#}")).ok();
    if let Some(s) = &_server {
        ui.set_installer_url(s.url.clone().into());
    }

    let weak = ui.as_weak();
    let _browser = pair::browse(pair::PAIR_SERVICE, move |f| {
        let _ = weak.upgrade_in_event_loop(move |ui| {
            FOUND.with_borrow_mut(|all| {
                match f {
                    Found::Up(m) => {
                        all.retain(|o| o.name != m.name);
                        all.push(m);
                    }
                    Found::Gone(n) => all.retain(|o| o.name != n),
                }
                all.sort_by(|a, b| a.name.cmp(&b.name));
                let rows: Vec<MacRow> = all
                    .iter()
                    .map(|m| MacRow { name: m.name.clone().into(), detail: format!("{}", m.addr.ip()).into() })
                    .collect();
                if let Some(vm) = ui.get_macs().as_any().downcast_ref::<VecModel<MacRow>>() {
                    vm.set_vec(rows);
                }
            });
        });
    })?;

    let weak = ui.as_weak();
    ui.on_pick(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(m) = FOUND.with_borrow(|all| all.get(i as usize).cloned()) else { return };
        choose(&ui, m);
    });

    let weak = ui.as_weak();
    ui.on_manual(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        match manual_mac(&text) {
            Some(m) => choose(&ui, m),
            None => ui.set_error(format!("Couldn't find “{text}”. Type its name (like my-mac.local) or IP address.").into()),
        }
    });

    let weak = ui.as_weak();
    ui.on_submit(move |code| {
        let Some(ui) = weak.upgrade() else { return };
        if pair::normalize_code(&code).is_none() {
            ui.set_error("A code has 8 letters and digits, like K7QF-M2XA.".into());
            return;
        }
        let Some(mac) = CHOSEN.with_borrow(|c| c.clone()) else { return };
        ui.set_error(SharedString::new());
        ui.set_page(3);
        let attempt = ATTEMPT.with_borrow_mut(|a| {
            *a += 1;
            *a
        });
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new().map_err(anyhow::Error::from).and_then(|rt| rt.block_on(pair::pair(&mac, &code)));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if ATTEMPT.with_borrow(|a| *a != attempt) {
                    return; // cancelled meanwhile
                }
                match result {
                    Ok(Outcome::Paired(p)) => {
                        let saved = Config::load_or_default().and_then(|mut cfg| {
                            let first = cfg.host.is_empty();
                            cfg.add_mac(p.clone(), first);
                            cfg.save()
                        });
                        if let Err(e) = saved {
                            ui.set_error(format!("Paired, but saving failed: {e:#}").into());
                            ui.set_page(2);
                            return;
                        }
                        ui.set_mac_name(p.name.into());
                        ui.set_page(4);
                    }
                    Ok(Outcome::Denied) => {
                        ui.set_error(format!("{} didn't allow this computer.", ui.get_mac_name()).into());
                        ui.set_page(2);
                    }
                    Err(e) => {
                        ui.set_error(format!("{e:#}").into());
                        ui.set_page(2);
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    ui.on_cancel_wait(move || {
        ATTEMPT.with_borrow_mut(|a| *a += 1);
        if let Some(ui) = weak.upgrade() {
            ui.set_page(2);
        }
    });

    let weak = ui.as_weak();
    ui.on_finish(move |use_now| {
        if use_now {
            if let Err(e) = use_paired_mac() {
                if let Some(ui) = weak.upgrade() {
                    ui.set_error(format!("{e:#}").into());
                }
                return;
            }
        }
        let _ = slint::quit_event_loop();
    });

    ui.run()?;
    Ok(())
}

fn choose(ui: &Wizard, m: FoundMac) {
    ui.set_mac_name(m.name.clone().into());
    ui.set_error(SharedString::new());
    CHOSEN.with_borrow_mut(|c| *c = Some(m));
    ui.set_page(2);
}

/// A Mac typed in by address ("my-mac.local", "192.168.1.20", optionally ":port").
fn manual_mac(text: &str) -> Option<FoundMac> {
    let text = text.trim();
    let with_port = if text.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        text.to_string()
    } else {
        format!("{text}:{}", crate::pair::PAIR_PORT)
    };
    let addrs: Vec<_> = with_port.to_socket_addrs().ok()?.collect();
    let addr = *addrs.iter().find(|a| a.is_ipv4()).or(addrs.first())?;
    let host = text.split(':').next().unwrap_or(text);
    Some(FoundMac {
        name: host.trim_end_matches(".local").to_string(),
        id: String::new(),
        hostname: if host.parse::<std::net::IpAddr>().is_ok() { String::new() } else { host.to_string() },
        addr,
        stream_port: 44120,
    })
}

/// Makes the newest paired Mac the one to stream from, and (re)starts the client for it.
fn use_paired_mac() -> Result<()> {
    let mut cfg = Config::load_or_default()?;
    if let Some(m) = cfg.macs.last().cloned() {
        cfg.activate(&m.fingerprint);
        cfg.save()?;
    }
    crate::setup::start_client()
}
