//! "Link a Mac": the pairing wizard (`sweeflow pair`). Its own process, so its event loop never
//! shares a thread with the streaming client's Wayland one.
//!
//! Install Sweeflow on the Mac → pick the Mac (found on the network, or typed by address) → the
//! Mac shows a 4-digit code, typed here → linked, and streaming from it straight away.

use std::{
    cell::RefCell,
    io::{BufRead, BufReader},
    net::ToSocketAddrs,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use anyhow::Result;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::sync::{mpsc, Notify};

use crate::{
    config::Config,
    pair::{self, Found, FoundMac, Outcome},
};

slint::slint! {
    import { ScrollView } from "std-widgets.slint";
    import { Look, Label, Dim, Btn, Field, Spin } from "look.slint";
    export { Look }

    export struct MacRow { name: string, detail: string }

    component Title inherits Text {
        color: Look.fg;
        font-size: 20px;
        font-weight: 600;
        wrap: word-wrap;
    }

    // One numbered step: its number, a title level with it, a line of detail, and anything else
    // below.
    component Step inherits HorizontalLayout {
        in property <string> n;
        in property <string> title;
        in property <string> detail;
        spacing: 12px;
        VerticalLayout {
            alignment: start;
            Rectangle {
                width: 24px;
                height: 24px;
                border-radius: Look.outlined ? Look.radius : 12px;
                background: Look.accent.transparentize(0.8);
                Text {
                    text: root.n;
                    color: Look.accent;
                    font-size: 12px;
                    font-weight: 700;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
            }
        }
        VerticalLayout {
            spacing: 6px;
            horizontal-stretch: 1;
            Text {
                text: root.title;
                color: Look.fg;
                font-size: 14px;
                font-weight: 600;
                height: 24px;
                vertical-alignment: center;
            }
            if root.detail != "": Dim { text: root.detail; }
            @children
        }
    }

    // A Mac found on the network, in the list.
    component MacEntry inherits Rectangle {
        in property <string> name;
        in property <string> detail;
        callback clicked;
        height: 52px;
        border-radius: Look.radius;
        border-width: Look.outlined ? 1px : 0px;
        border-color: Look.line;
        background: Look.fg.transparentize(touch.has-hover ? 0.88 : 0.94);
        touch := TouchArea {
            mouse-cursor: pointer;
            clicked => { root.clicked(); }
        }
        HorizontalLayout {
            padding-left: 14px;
            padding-right: 14px;
            spacing: 10px;
            VerticalLayout {
                alignment: center;
                spacing: 2px;
                Text { text: root.name; color: Look.fg; font-size: 14px; font-weight: 600; }
                Text { text: root.detail; color: Look.muted; font-size: 11px; }
            }
            Rectangle {}
            Text { text: "›"; color: Look.muted; font-size: 22px; vertical-alignment: center; }
        }
    }

    // Where the Mac's code is typed: big digits, centred.
    component CodeField inherits Rectangle {
        in-out property <string> text <=> input.text;
        in property <bool> enabled: true;
        callback accepted(string);
        width: 200px;
        height: 60px;
        border-radius: Look.radius;
        border-width: 1px;
        border-color: input.has-focus ? Look.accent : Look.line;
        background: Look.surface;
        forward-focus: input;
        init => { input.focus(); }
        input := TextInput {
            x: 10px;
            width: parent.width - 20px;
            height: parent.height;
            single-line: true;
            enabled: root.enabled;
            horizontal-alignment: center;
            vertical-alignment: center;
            color: Look.fg;
            font-size: 32px;
            font-family: Look.mono;
            letter-spacing: 8px;
            input-type: number;
            selection-background-color: Look.accent;
            selection-foreground-color: Look.on-accent;
            accepted => { root.accepted(self.text); }
        }
    }

    export component Wizard inherits Window {
        title: "Link a Mac";
        icon: @image-url("../../assets/sweeflow-app.svg");
        background: Look.bg;
        default-font-family: Look.font;
        // A fixed size: a dialog, which tiling window managers leave floating.
        width: 560px;
        // Less on a short screen (see look::fit_to_screen): the page scrolls.
        in property <length> room: 10000px;
        height: min(tailscale-url != "" ? 564px : 540px, room);

        in-out property <int> page: 0;
        in property <string> installer-url;
        // The same page over Tailscale, when this computer is on a tailnet.
        in property <string> tailscale-url;
        // A firewall (ufw) is on and may block the page: offer to let it through.
        in property <bool> firewall-on;
        in-out property <string> firewall-note;
        callback open-firewall();
        in property <[MacRow]> macs;
        in property <string> mac-name;
        // The Mac is showing its code (until then, it's being asked).
        in property <bool> code-ready;
        // A code was sent, and the Mac's answer is awaited.
        in property <bool> checking;
        in-out property <string> code-text;
        // Under the code: a wrong one, or how many digits it has.
        in property <string> code-note;
        in property <bool> code-wrong;
        in-out property <string> error;
        // The Mac that was in use before this one was linked, if any.
        in property <string> switched-from;

        callback pick(int);
        callback manual(string);
        callback cancel-wait();
        callback submit-code(string);
        callback finish();

        sv := ScrollView {
          content-width: box.width;
          content-height: box.height;
          box := VerticalLayout {
            width: sv.visible-width;
            height: max(self.preferred-height, sv.visible-height);
            padding: 24px;

            if page == 0: VerticalLayout {
                spacing: 18px;
                HorizontalLayout {
                    spacing: 14px;
                    alignment: start;
                    Image { source: @image-url("../../assets/sweeflow-app.svg"); width: 48px; height: 48px; }
                    VerticalLayout {
                        alignment: center;
                        spacing: 2px;
                        Title { text: "Link a Mac"; }
                        Dim { text: "Install Sweeflow on your Mac, then link it to this computer."; }
                    }
                }
                Step {
                    n: "1";
                    title: "Download Sweeflow on your Mac";
                    detail: installer-url != "" ? "Open this address in Safari on your Mac:" : "Copy Sweeflow.dmg to your Mac.";
                    if installer-url != "": Rectangle {
                        height: tailscale-url != "" ? 64px : 40px;
                        border-radius: Look.radius;
                        border-width: Look.outlined ? 1px : 0px;
                        border-color: Look.line;
                        background: Look.surface;
                        VerticalLayout {
                            alignment: center;
                            spacing: 4px;
                            Text {
                                text: installer-url;
                                color: Look.fg;
                                font-size: 16px;
                                font-family: Look.mono;
                                horizontal-alignment: center;
                            }
                            if tailscale-url != "": Text {
                                text: "Over Tailscale: " + tailscale-url;
                                color: Look.muted;
                                font-size: 12px;
                                font-family: Look.mono;
                                horizontal-alignment: center;
                            }
                        }
                    }
                    if installer-url != "" && !firewall-on: Dim {
                        text: "If the page doesn't load, this computer's firewall may be blocking port 44130.";
                    }
                    // A fixed height (three lines of text fit, as Omarchy's wider font needs):
                    // wrapped text beside a button doesn't report its height to the layout, and
                    // it ran into the next step.
                    if installer-url != "" && firewall-on: Rectangle {
                        height: 66px;
                        border-radius: Look.radius;
                        border-width: Look.outlined ? 1px : 0px;
                        border-color: Look.line;
                        background: Look.surface;
                        HorizontalLayout {
                            padding-left: 12px;
                            padding-right: 11px;
                            spacing: 12px;
                            Dim {
                                text: firewall-note != "" ? firewall-note : "This computer's firewall is blocking that page. You can allow it while this window is open.";
                                vertical-alignment: center;
                                horizontal-stretch: 1;
                            }
                            if firewall-note == "": VerticalLayout {
                                alignment: center;
                                Btn { text: "Allow"; clicked => { open-firewall(); } }
                            }
                        }
                    }
                }
                Step {
                    n: "2";
                    title: "Install it and open it";
                    detail: "Drag Sweeflow into Applications and open it. If macOS refuses to open it, go to System Settings → Privacy & Security and click Open Anyway.";
                }
                Step {
                    n: "3";
                    title: "Follow its setup";
                    detail: "Sweeflow asks for the permissions it needs. Leave it running on your Mac and continue here.";
                }
                Rectangle {}
                HorizontalLayout {
                    alignment: end;
                    Btn { text: "Continue"; primary: true; clicked => { page = 1; } }
                }
            }

            if page == 1: VerticalLayout {
                spacing: 14px;
                VerticalLayout {
                    spacing: 4px;
                    Title { text: "Choose your Mac"; }
                    Dim { text: "These Macs are running Sweeflow on this network. Choose yours, and it will show a code to type here."; }
                }
                // The Macs found on this network, or the wait for one.
                Rectangle {
                    vertical-stretch: 1;
                    border-radius: Look.radius;
                    border-width: Look.outlined ? 1px : 0px;
                    border-color: Look.line;
                    background: Look.surface;
                    if macs.length == 0: VerticalLayout {
                        alignment: center;
                        spacing: 10px;
                        HorizontalLayout { alignment: center; Spin { size: 22px; } }
                        Dim { text: "Looking for your Mac on this network…"; horizontal-alignment: center; }
                    }
                    if macs.length > 0: ScrollView {
                        VerticalLayout {
                            padding: 6px;
                            spacing: 6px;
                            alignment: start;
                            for m[i] in macs: MacEntry { name: m.name; detail: m.detail; clicked => { root.pick(i); } }
                        }
                    }
                }
                VerticalLayout {
                    spacing: 8px;
                    Dim { text: "If your Mac isn't listed, type its name or IP address:"; }
                    HorizontalLayout {
                        spacing: 8px;
                        // Its examples: a Tailscale address, a name, a virtual machine with its ports forwarded.
                        addr := Field { placeholder: "100.101.102.103, my-mac, localhost"; accepted(t) => { root.manual(t); } }
                        Btn { text: "Use address"; enabled: addr.text != ""; clicked => { root.manual(addr.text); } }
                    }
                    if error != "": Label { text: error; color: Look.danger; }
                }
                HorizontalLayout {
                    alignment: start;
                    Btn { text: "Back"; clicked => { error = ""; page = 0; } }
                }
            }

            if page == 2: VerticalLayout {
                spacing: 16px;
                alignment: center;
                Title {
                    text: code-ready ? "Enter the code shown on " + mac-name : "Asking " + mac-name + "…";
                    horizontal-alignment: center;
                }
                if !code-ready: HorizontalLayout { alignment: center; Spin { size: 28px; } }
                if code-ready: HorizontalLayout {
                    alignment: center;
                    CodeField {
                        text <=> root.code-text;
                        enabled: !checking;
                        accepted(t) => { root.submit-code(t); }
                    }
                }
                Label {
                    // Only news here (a wrong code, the Mac stopped it): the title already says where the code is.
                    text: code-note;
                    color: code-wrong ? Look.danger : Look.muted;
                    horizontal-alignment: center;
                }
                HorizontalLayout {
                    alignment: center;
                    spacing: 8px;
                    Btn { text: "Cancel"; clicked => { root.cancel-wait(); } }
                    if code-ready: Btn {
                        text: "Link";
                        primary: true;
                        enabled: !checking && code-text != "";
                        clicked => { root.submit-code(root.code-text); }
                    }
                }
            }

            if page == 3: VerticalLayout {
                spacing: 16px;
                HorizontalLayout {
                    spacing: 14px;
                    alignment: start;
                    Image { source: @image-url("../../assets/sweeflow-app.svg"); width: 48px; height: 48px; }
                    Title { text: "Linked to " + mac-name; vertical-alignment: center; }
                }
                Dim {
                    text: (switched-from != "" ? "Switched from " + switched-from + ". " : "")
                        + "Its apps will open on this desktop in windows of their own.";
                }
                if error != "": Label { text: error; color: Look.danger; }
                Rectangle {}
                HorizontalLayout {
                    alignment: end;
                    Btn { text: "Done"; primary: true; clicked => { root.finish(); } }
                }
            }
          }
        }
    }
}

thread_local! {
    /// Macs that take link requests right now (UI thread only).
    static FOUND: RefCell<Vec<FoundMac>> = const { RefCell::new(Vec::new()) };
    /// Bumped to forget a link request the person cancelled.
    static ATTEMPT: RefCell<u32> = const { RefCell::new(0) };
    /// The link request under way: what cancels it, and its thread.
    static LINKING: RefCell<Option<(Arc<Notify>, JoinHandle<()>)>> = const { RefCell::new(None) };
    /// Where the code typed goes, while the Mac shows one.
    static CODES: RefCell<Option<mpsc::UnboundedSender<String>>> = const { RefCell::new(None) };
}

pub fn run() -> Result<()> {
    // One "Link a Mac" window at a time (the launcher, the tray and a first start all open it).
    let Some(_only) = crate::lock("sweeflow-pair.lock")? else {
        log::info!("the Link a Mac window is already open");
        return Ok(());
    };
    let ui = Wizard::new()?;
    crate::look::apply!(ui);
    // Matches sweeflow.desktop (written by `sweeflow install`), which gives KDE the logo for
    // the title bar and taskbar. After the window is made (set before, it was refused, and the
    // title bar showed a placeholder icon), and before it's shown.
    if let Err(e) = slint::set_xdg_app_id("sweeflow") {
        log::warn!("app id: {e}");
    }
    let model = Rc::new(VecModel::<MacRow>::default());
    ui.set_macs(ModelRc::from(model.clone()));

    let _server = crate::installer::serve().inspect_err(|e| log::info!("installer not served: {e:#}")).ok();
    if let Some(s) = &_server {
        ui.set_installer_url(s.url.clone().into());
        ui.set_tailscale_url(s.tailscale_url.clone().unwrap_or_default().into());
    }
    // The window has one size. GNOME keeps to that (with the title bar's double click fixed, see
    // vendor/sctk-adwaita); Hyprland maximises any window it is told to (the button in Omarchy's
    // title bars), so there it is put straight back.
    crate::hyprland::keep_window_size();

    // CachyOS and others ship ufw switched on: the Mac can't reach the page until it's allowed.
    // The opening lasts while this window is open, so it's offered again each time.
    let _ = std::fs::remove_file(crate::config::dir().join("firewall-allowed"));
    ui.set_firewall_on(_server.is_some() && firewall_active());
    let opening: Arc<Mutex<Option<Child>>> = Arc::default();
    let weak_fw = ui.as_weak();
    let opening_fw = opening.clone();
    ui.on_open_firewall(move || {
        let weak = weak_fw.clone();
        let opening = opening_fw.clone();
        std::thread::spawn(move || {
            let note = match allow_installer_port() {
                Ok(child) => {
                    *opening.lock().unwrap() = Some(child);
                    "The page is allowed until this window closes. Open the address on your Mac now."
                }
                Err(e) => {
                    log::warn!("firewall: {e:#}");
                    "The firewall wasn't changed. To load the page, allow TCP port 44130 in its settings."
                }
            };
            let _ = weak.upgrade_in_event_loop(move |ui| ui.set_firewall_note(note.into()));
        });
    });

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
                    .map(|m| MacRow {
                        name: m.name.clone().into(),
                        detail: if already_linked(m) { "Already linked".into() } else { format!("{}", m.addr.ip()).into() },
                    })
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
        if already_linked(&m) {
            ui.set_error(format!("{} is already linked to this computer. To link it again, unlink it first.", m.name).into());
            return;
        }
        start_link(&ui, m);
    });

    let weak = ui.as_weak();
    ui.on_manual(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        match manual_mac(&text) {
            Some(m) => start_link(&ui, m),
            None => ui.set_error(format!("Couldn't find “{text}”. Type the Mac's name, such as my-mac.local, or its IP address.").into()),
        }
    });

    let weak = ui.as_weak();
    ui.on_cancel_wait(move || {
        cancel_link();
        if let Some(ui) = weak.upgrade() {
            ui.set_page(1);
        }
    });

    let weak = ui.as_weak();
    ui.on_submit_code(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        // One code per try: a second Enter while it's checked would spend the next try.
        if ui.get_checking() {
            return;
        }
        let Some(code) = pair::normalize_code(&text) else {
            ui.set_code_note(format!("The code has {} digits.", pair::CODE_DIGITS).into());
            ui.set_code_wrong(true);
            return;
        };
        let sent = CODES.with_borrow(|c| c.as_ref().is_some_and(|tx| tx.send(code).is_ok()));
        if sent {
            ui.set_checking(true);
            ui.set_code_wrong(false);
            ui.set_code_note("Checking…".into());
        }
    });

    ui.on_finish(|| {
        let _ = slint::quit_event_loop();
    });

    crate::look::fit_to_screen_soon(&ui, |ui, h| ui.set_room(h));
    let shown = ui.run();
    // Closed while asking a Mac: say so there (it takes its question down) before leaving.
    if let Some((_, thread)) = cancel_link() {
        let _ = thread.join();
    }
    // Closing the helper's input takes the firewall rule back out.
    if let Some(mut child) = opening.lock().unwrap().take() {
        drop(child.stdin.take());
        let _ = child.wait();
    }
    shown?;
    Ok(())
}

/// One of this computer's linked Macs (by its certificate, whatever address it's found at).
fn already_linked(m: &FoundMac) -> bool {
    !m.id.is_empty() && Config::load_or_default().is_ok_and(|c| c.macs.iter().any(|p| p.fingerprint.starts_with(&m.id)))
}

/// Forgets the link request under way, if any, and tells its thread to stop asking.
fn cancel_link() -> Option<(Arc<Notify>, JoinHandle<()>)> {
    ATTEMPT.with_borrow_mut(|a| *a += 1);
    CODES.with_borrow_mut(|c| *c = None);
    let linking = LINKING.with_borrow_mut(Option::take);
    if let Some((cancel, _)) = &linking {
        cancel.notify_one();
    }
    linking
}

/// Asks `mac` to link, on the page where its code is typed. The answer leads to the last page
/// (streaming from it already), or back to the list with what went wrong.
fn start_link(ui: &Wizard, mac: FoundMac) {
    cancel_link();
    ui.set_code_ready(false);
    ui.set_checking(false);
    ui.set_code_text(SharedString::new());
    ui.set_code_note(SharedString::new());
    ui.set_code_wrong(false);
    let (codes_tx, mut codes) = mpsc::unbounded_channel();
    CODES.with_borrow_mut(|c| *c = Some(codes_tx));
    // A Mac typed in by address is called by its address until it answers with its name
    // (when linked): "your Mac" reads better than an IP address meanwhile.
    let typed = mac.name.parse::<std::net::IpAddr>().is_ok() || mac.name == "localhost" || mac.name.contains(':');
    ui.set_mac_name(if typed { "your Mac".into() } else { mac.name.clone().into() });
    ui.set_error(SharedString::new());
    ui.set_page(2);
    let attempt = ATTEMPT.with_borrow(|a| *a);
    let cancel = Arc::new(Notify::new());
    let cancelled = cancel.clone();
    let weak = ui.as_weak();
    let thread = std::thread::spawn(move || {
        let weak_ask = weak.clone();
        let on_ask = move |tries_left: u8, wrong: bool| {
            let _ = weak_ask.upgrade_in_event_loop(move |ui| {
                if ATTEMPT.with_borrow(|a| *a != attempt) {
                    return;
                }
                ui.set_code_ready(true);
                ui.set_checking(false);
                ui.set_code_text(SharedString::new());
                ui.set_code_wrong(wrong);
                let tries = if tries_left == 1 { "1 try".to_string() } else { format!("{tries_left} tries") };
                ui.set_code_note(if wrong { format!("That's not the code. {tries} left.").into() } else { SharedString::new() });
            });
        };
        // A Mac that's asleep, locked or without Sweeflow open is waited for: asked again until
        // it answers (or the person cancels), rather than turned away to ask again by hand.
        let weak_wait = weak.clone();
        let waiting = move |why: SharedString| {
            let _ = weak_wait.upgrade_in_event_loop(move |ui| {
                if ATTEMPT.with_borrow(|a| *a == attempt) && !ui.get_code_ready() {
                    ui.set_code_wrong(false);
                    ui.set_code_note(why);
                }
            });
        };
        let result = tokio::runtime::Runtime::new().map_err(anyhow::Error::from).and_then(|rt| {
            rt.block_on(async {
                loop {
                    let outcome = pair::link(&mac, &cancelled, &mut codes, on_ask.clone()).await?;
                    let why = match outcome {
                        Outcome::Unreachable => "It isn't answering yet. Waiting for it: wake it, and check Sweeflow is open on it.",
                        Outcome::Locked => "It's locked. Waiting for it: unlock it to see the code.",
                        other => return Ok(other),
                    };
                    waiting(why.into());
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                        _ = cancelled.notified() => return Ok(Outcome::Cancelled),
                    }
                }
            })
        });
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if ATTEMPT.with_borrow(|a| *a != attempt) {
                return; // cancelled meanwhile
            }
            LINKING.with_borrow_mut(|l| *l = None);
            CODES.with_borrow_mut(|c| *c = None);
            let name = ui.get_mac_name();
            match result {
                Ok(Outcome::Linked(p)) => {
                    let saved = Config::load_or_default().and_then(|mut cfg| {
                        let before = cfg.macs.iter().find(|m| m.fingerprint == cfg.host_fingerprint).map(|m| m.name.clone());
                        cfg.add_mac(p.clone(), true);
                        cfg.save().map(|()| before)
                    });
                    let before = match saved {
                        Ok(b) => b,
                        Err(e) => {
                            ui.set_error(format!("Linked, but the settings couldn't be saved: {e:#}").into());
                            ui.set_page(1);
                            return;
                        }
                    };
                    ui.set_switched_from(before.unwrap_or_default().into());
                    ui.set_mac_name(p.name.into());
                    ui.set_error(SharedString::new());
                    if let Err(e) = after_first_link(&p.fingerprint) {
                        log::warn!("starting to stream from the Mac just linked: {e:#}");
                        ui.set_error(format!("Couldn't start streaming from it: {e:#}").into());
                    }
                    ui.set_page(3);
                }
                Ok(other) => {
                    ui.set_error(other.message(&name).unwrap_or_default().into());
                    ui.set_page(1);
                }
                Err(e) => {
                    log::warn!("linking to {name}: {e:#}");
                    ui.set_error(format!("Linking to {name} didn't work ({e:#}).").into());
                    ui.set_page(1);
                }
            }
        });
    });
    LINKING.with_borrow_mut(|l| *l = Some((cancel, thread)));
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
        busy: false,
        locked: false,
    })
}

/// A Mac was just linked (and made the one to use in the config): streams from it straight away.
/// A running client switches itself over (telling the Mac it leaves that it's going, see
/// `tray::use_mac`); otherwise the client is started.
fn after_first_link(fingerprint: &str) -> Result<()> {
    const BUS_NAME: &str = "dev.sweeflow.Client";
    let bus = zbus::blocking::Connection::session().ok();
    let running = bus.as_ref().is_some_and(|bus| {
        bus.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &(BUS_NAME,))
            .ok()
            .and_then(|r| r.body().deserialize::<bool>().ok())
            .unwrap_or(false)
    });
    if let (true, Some(bus)) = (running, &bus) {
        bus.call_method(Some(BUS_NAME), "/dev/sweeflow/Panel", Some("dev.sweeflow.Panel"), "Do", &("use", fingerprint))?;
        return Ok(());
    }
    // (A Mac streaming to another computer meanwhile answers Busy: "In use by …" until it's free.)
    crate::net::set_offline(false);
    crate::setup::start_client()
}

/// ufw is switched on (it then blocks incoming connections by default). Ubuntu and Mint keep
/// ufw.service "active" even with the firewall off (the unit only runs ufw's boot script), so
/// the switch itself is read: `ENABLED=yes` in ufw.conf, which anyone can read.
fn firewall_active() -> bool {
    let service = Command::new("systemctl")
        .args(["is-active", "--quiet", "ufw"])
        .status()
        .is_ok_and(|s| s.success());
    service && std::fs::read_to_string("/etc/ufw/ufw.conf").is_ok_and(|s| ufw_enabled(&s))
}

fn ufw_enabled(conf: &str) -> bool {
    conf.lines()
        .filter_map(|l| l.trim().strip_prefix("ENABLED="))
        .last()
        .is_some_and(|v| v.trim().trim_matches(['"', '\'']).eq_ignore_ascii_case("yes"))
}

/// Lets the local network reach the installer page for a while: one ufw rule, for this
/// computer's network only, asked for with the usual password prompt (pkexec). The helper keeps
/// running as root and deletes the rule when its input closes: when the window closes, or if
/// Sweeflow dies, so it's never left open.
fn allow_installer_port() -> anyhow::Result<Child> {
    let net = local_network().ok_or_else(|| anyhow::anyhow!("couldn't tell which network this is"))?;
    let port = crate::installer::PORT.to_string();
    // The local network, and Tailscale's addresses when this computer is on a tailnet.
    let tailnet = if crate::installer::tailscale_ip().is_some() { "100.64.0.0/10" } else { "" };
    let script = r#"for net in $1 $3; do
  ufw allow from $net to any port $2 proto tcp comment 'Sweeflow installer' >/dev/null || exit 1
done
echo open
cat >/dev/null
for net in $1 $3; do
  ufw delete allow from $net to any port $2 proto tcp >/dev/null
done"#;
    let mut child = Command::new("pkexec")
        .args(["sh", "-c", script, "sh", &net, &port, tailnet])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
    if line.trim() != "open" {
        let _ = child.wait();
        anyhow::bail!("the firewall wasn't changed");
    }
    Ok(child)
}

/// This computer's local network as an address range ("192.168.1.0/24"), from `ip`.
fn local_network() -> Option<String> {
    let out = Command::new("ip").args(["-o", "-4", "route", "show", "scope", "link"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|n| n.contains('/') && !n.starts_with("169.254."))
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::ufw_enabled;

    #[test]
    fn ufw_switch() {
        assert!(!ufw_enabled("# comment\nENABLED=no\nLOGLEVEL=low\n"));
        assert!(ufw_enabled("ENABLED=yes\n"));
        assert!(ufw_enabled("ENABLED=\"yes\"\n"));
        assert!(!ufw_enabled("#ENABLED=yes\n"));
        assert!(!ufw_enabled(""));
    }
}
