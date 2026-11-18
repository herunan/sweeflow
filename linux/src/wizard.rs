//! "Link a Mac": the pairing wizard (`sweeflow pair`). Its own process, so its event loop never
//! shares a thread with the streaming client's Wayland one.
//!
//! Install Sweeflow on the Mac → pick the Mac (found on the network, or typed by address) → the
//! Mac shows a 4-digit code, typed here → linked, and streaming from it straight away.

use std::{cell::RefCell, net::ToSocketAddrs, rc::Rc, sync::Arc, thread::JoinHandle};

use anyhow::Result;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::sync::{mpsc, Notify};

use crate::{
    config::Config,
    pair::{self, Found, FoundMac, Outcome},
};

slint::slint! {
    import { ScrollView } from "std-widgets.slint";
    import { Look, Label, Dim, Btn, Field, Spin, MacScene, KeyMapping, KeyCaps } from "look.slint";
    export { Look }

    export struct MacRow { name: string, detail: string }

    component Title inherits Text {
        color: Look.fg;
        font-size: 20px;
        font-weight: 600;
        wrap: word-wrap;
    }

    // A small picture in an accent tile: an SVG path on a 24×24 grid, drawn as lines (or filled).
    component Glyph inherits Rectangle {
        in property <string> d;
        in property <bool> filled;
        in property <length> size: 30px;
        // A colour of its own, the picture in white (as in the Mac's settings); else the accent's tint.
        in property <bool> colorful;
        in property <color> tint;
        width: root.size;
        height: root.size;
        border-radius: Look.outlined ? Look.radius : root.size * 0.3;
        background: root.colorful ? @linear-gradient(180deg, root.tint.brighter(0.15) 0%, root.tint.darker(0.15) 100%) : Look.accent.transparentize(0.82);
        Path {
            width: root.size * 0.58;
            height: root.size * 0.58;
            viewbox-width: 24;
            viewbox-height: 24;
            commands: root.d;
            stroke: root.filled ? transparent : root.colorful ? white : Look.accent;
            stroke-width: 1.8px;
            fill: root.filled ? (root.colorful ? white : Look.accent) : transparent;
        }
    }

    // One of Sweeflow's keys: the key in a chip, what it does.
    component KeyLine inherits HorizontalLayout {
        in property <[string]> key;
        in property <string> what;
        spacing: 8px;
        // The keys in a column of one width, so what they do lines up.
        HorizontalLayout { width: 112px; KeyCaps { caps: root.key; } }
        Label { text: root.what; vertical-alignment: center; }
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

    // A piece of confetti drifting down the welcome banner, over and over.
    component Confetti inherits Rectangle {
        in property <length> left;
        in property <float> phase;
        in property <length> fall;
        in property <angle> spin;
        property <float> t: mod(animation-tick() / 4200ms + root.phase, 1);
        in property <color> tint;
        x: root.left + 8px * sin(root.t * 720deg);
        y: -14px + (root.fall + 14px) * root.t;
        width: 7px;
        height: 11px;
        Rectangle {
            border-radius: 2px;
            background: root.tint;
            transform-rotation: root.spin + root.t * 540deg;
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
        height: min(636px, room);

        in-out property <int> page: 0;
        // Where the Mac downloads Sweeflow.
        in property <string> download-site: "sweeflow.app";
        callback open-site();
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
        // The keyboard shortcuts, as the desktop has them now ("" for one removed), shown once
        // linked; and whether the desktop has a settings page to change them in.
        in property <[string]> key-open;
        in property <[string]> key-pause;
        in property <[string]> key-stop;
        in property <[string]> key-menu;
        in property <bool> shortcut-settings;
        callback change-shortcuts();
        // The name typed for the Mac just linked; it was renamed to it.
        in-out property <string> new-name;
        in-out property <bool> renamed;
        // The Mac's name is being changed (the banner's pencil).
        in-out property <bool> editing-name;
        // Hyprland (and Omarchy on it): its own keys stay first over Mac windows.
        in property <bool> hyprland;
        in property <bool> omarchy;
        // A Mac-style key remapper (Toshy): keys written the Mac's way, no mapping line.
        in property <bool> mac-style;
        // Alt and Super swapped in Mac windows (config::swap_cmd_opt).
        in property <bool> swapped;
        callback rename(string);


        sv := ScrollView {
          content-width: box.width;
          content-height: box.height;
          box := VerticalLayout {
            width: sv.visible-width;
            height: max(self.preferred-height, sv.visible-height);
            padding: 24px;

            // Getting the Mac ready, in three pictures, and the Macs found: one page.
            if page == 0: VerticalLayout {
                spacing: 16px;
                HorizontalLayout {
                    spacing: 12px;
                    alignment: start;
                    Image { source: @image-url("../../assets/sweeflow-app.svg"); width: 40px; height: 40px; }
                    VerticalLayout {
                        alignment: center;
                        Title { text: "Link a Mac"; }
                    }
                }
                // What to do on the Mac first, in one go.
                Rectangle {
                    border-radius: Look.radius + 2px;
                    border-width: Look.outlined ? 1px : 0px;
                    border-color: Look.line;
                    background: Look.surface;
                    HorizontalLayout {
                        padding: 14px;
                        spacing: 12px;
                        VerticalLayout {
                            alignment: start;
                            Glyph { d: "M 12 3 L 12 15 M 7 10 L 12 15 L 17 10 M 4 20 L 20 20"; size: 34px; }
                        }
                        VerticalLayout {
                            spacing: 4px;
                            horizontal-stretch: 1;
                            HorizontalLayout {
                                alignment: start;
                                Text { text: "On your Mac, download Sweeflow from "; color: Look.fg; font-size: 13px; vertical-alignment: center; }
                                Rectangle { width: 2px; }
                                // A violet pill, brighter under the pointer.
                                Rectangle {
                                    width: site.preferred-width + 18px;
                                    height: 22px;
                                    border-radius: 11px;
                                    background: link.has-hover ? Look.accent.brighter(0.15) : Look.accent;
                                    site := Text {
                                        text: root.download-site;
                                        color: white;
                                        font-size: 13px;
                                        font-weight: 600;
                                        horizontal-alignment: center;
                                        vertical-alignment: center;
                                    }
                                    link := TouchArea {
                                        mouse-cursor: pointer;
                                        clicked => { root.open-site(); }
                                    }
                                }
                            }
                            // The Mac window's name in bold.
                            HorizontalLayout {
                                alignment: start;
                                Dim { text: "Install it, allow all permissions and reach the "; font-size: 12px; wrap: no-wrap; }
                                Text { text: "Link a Linux machine"; color: Look.fg; font-size: 12px; font-weight: 700; }
                                Dim { text: " screen."; font-size: 12px; wrap: no-wrap; }
                            }
                        }
                    }
                }
                // The Macs found on this network, or the wait for one.
                Rectangle {
                    vertical-stretch: 1;
                    min-height: 96px;
                    border-radius: Look.radius;
                    border-width: Look.outlined ? 1px : 0px;
                    border-color: Look.line;
                    background: Look.surface;
                    if macs.length == 0: VerticalLayout {
                        alignment: center;
                        spacing: 10px;
                        HorizontalLayout { alignment: center; Spin { size: 20px; } }
                        Dim { text: "Looking for your Mac…"; horizontal-alignment: center; }
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
                    HorizontalLayout {
                        spacing: 8px;
                        // A name, an IP address (Tailscale's too), or localhost for a virtual machine with its ports forwarded.
                        addr := Field { placeholder: "Not listed? Its name or IP address (Tailscale, VM)"; accepted(t) => { root.manual(t); } }
                        Btn { text: "Link"; enabled: addr.text != ""; clicked => { root.manual(addr.text); } }
                    }
                    if error != "": Label { text: error; color: Look.danger; }
                }
            }

            if page == 1: VerticalLayout {
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

            // Linked, and streaming: a welcome (the Mac renamed from there), what Sweeflow does,
            // its keys, and where Mac shortcuts go.
            if page == 2: VerticalLayout {
                spacing: 14px;
                Rectangle {
                    height: max(64px, hello.preferred-height);
                    border-radius: Look.radius + 4px;
                    clip: true;
                    background: @linear-gradient(135deg, Look.accent 0%, Look.accent.darker(0.5) 100%);
                    for c in [
                        { left: 0.06, phase: 0.05, tint: #fbbf24, spin: 20deg },
                        { left: 0.15, phase: 0.55, tint: #f472b6, spin: 70deg },
                        { left: 0.24, phase: 0.30, tint: #ffffff, spin: 140deg },
                        { left: 0.33, phase: 0.80, tint: #34d399, spin: 200deg },
                        { left: 0.62, phase: 0.15, tint: #60a5fa, spin: 260deg },
                        { left: 0.71, phase: 0.65, tint: #fbbf24, spin: 320deg },
                        { left: 0.80, phase: 0.40, tint: #ffffff, spin: 30deg },
                        { left: 0.89, phase: 0.90, tint: #f472b6, spin: 110deg },
                        { left: 0.95, phase: 0.25, tint: #34d399, spin: 170deg },
                    ]: Confetti {
                        left: parent.width * c.left;
                        phase: c.phase;
                        fall: parent.height;
                        tint: c.tint;
                        spin: c.spin;
                    }
                    hello := HorizontalLayout {
                        padding: 12px;
                        padding-left: 14px;
                        spacing: 12px;
                        Rectangle {
                            width: 40px;
                            height: 40px;
                            border-radius: 11px;
                            background: #ffffff30;
                            Image { source: @image-url("../../assets/sweeflow-app.svg"); width: 32px; height: 32px; }
                        }
                        VerticalLayout {
                            alignment: center;
                            spacing: 1px;
                            Text { text: "Welcome to your new home,"; color: white; font-size: 16px; font-weight: 600; }
                            // The Mac's name, renamed in place: the pencil opens a field.
                            if !root.editing-name: HorizontalLayout {
                                spacing: 8px;
                                alignment: start;
                                Text { text: mac-name + "!"; color: white; font-size: 16px; font-weight: 600; wrap: word-wrap; }
                                Rectangle {
                                    width: rename-row.preferred-width + 20px;
                                    height: 26px;
                                    y: (parent.height - self.height) / 2;
                                    border-radius: 13px;
                                    background: pencil.has-hover ? #ffffff40 : #ffffff22;
                                    rename-row := HorizontalLayout {
                                        x: 10px;
                                        spacing: 5px;
                                        Path {
                                            width: 12px;
                                            height: 12px;
                                            y: (26px - 12px) / 2;
                                            viewbox-width: 24;
                                            viewbox-height: 24;
                                            commands: "M 4 20 L 4 16 L 15 5 L 19 9 L 8 20 Z M 13 7 L 17 11";
                                            stroke: white;
                                            stroke-width: 1.6px;
                                        }
                                        Text { text: "Rename"; color: white; font-size: 12px; vertical-alignment: center; }
                                    }
                                    pencil := TouchArea {
                                        mouse-cursor: pointer;
                                        clicked => { root.new-name = root.mac-name; root.editing-name = true; }
                                    }
                                }
                            }
                            if root.editing-name: HorizontalLayout {
                                spacing: 8px;
                                Field { text <=> root.new-name; accepted(t) => { root.rename(t); root.editing-name = false; } }
                                Btn {
                                    text: "Save";
                                    enabled: root.new-name != "";
                                    clicked => { root.rename(root.new-name); root.editing-name = false; }
                                }
                            }
                            if switched-from != "": Text {
                                text: "Switched over from " + switched-from + ".";
                                color: #ffffffc0;
                                font-size: 12px;
                                wrap: word-wrap;
                            }
                        }
                    }
                }
                if error != "": Label { text: error; color: Look.danger; }
                // What Sweeflow does: a strip of pictures, each with its words under it.
                strip := HorizontalLayout {
                    vertical-stretch: 0;
                    padding-top: 6px;
                    spacing: 4px;
                    for f in [
                        { d: "M 13 2 L 4 14 L 11 14 L 10 22 L 20 10 L 13 10 Z", filled: true, title: "Fast, sharp and smooth" },
                        { d: "M 4 4 L 10 4 L 10 10 L 4 10 Z M 14 4 L 20 4 L 20 10 L 14 10 Z M 4 14 L 10 14 L 10 20 L 4 20 Z M 14 14 L 20 14 L 20 20 L 14 20 Z", filled: false, title: "Apps in launcher" },
                        { d: "M 3 5 L 21 5 M 6 10 L 18 10 M 6 14 L 18 14 M 6 18 L 13 18", filled: false, title: "Global menu" },
                        { d: "M 9 4 L 6 4 L 6 21 L 18 21 L 18 4 L 15 4 M 9 2 L 15 2 L 15 6 L 9 6 Z", filled: false, title: "Shared clipboard" },
                        { d: "M 6 2 L 14 2 L 19 7 L 19 22 L 6 22 Z M 14 2 L 14 7 L 19 7 M 9 15 L 16 15 M 13 12 L 16 15 L 13 18", filled: false, title: "Drag and drop files" },
                        { d: "M 4 9 L 8 9 L 13 5 L 13 19 L 8 15 L 4 15 Z M 16 9 Q 18 12 16 15 M 18.5 6.5 Q 22.5 12 18.5 17.5", filled: false, title: "Sound passthrough" },
                        { d: "M 12 2 A 10 10 0 1 0 12 22 A 10 10 0 1 0 12 2 Z M 2 12 L 22 12 M 12 2 Q 7 12 12 22 M 12 2 Q 17 12 12 22", filled: false, title: "Local, remote or VM" },
                    ]: VerticalLayout {
                        // All one width (the words wrap, three lines at most).
                        width: (strip.width - 6 * 4px) / 7;
                        spacing: 7px;
                        alignment: start;
                        HorizontalLayout { alignment: center; Glyph { d: f.d; filled: f.filled; size: 34px; } }
                        Text { text: f.title; height: 44px; color: Look.fg; font-size: 11px; horizontal-alignment: center; wrap: word-wrap; }
                    }
                }
                Rectangle { height: 4px; }
                // Sweeflow's keys, one under the other.
                VerticalLayout {
                    vertical-stretch: 0;
                    spacing: 8px;
                    Text { text: "Sweeflow shortcuts"; color: Look.fg; font-size: 13px; font-weight: 700; }
                    KeyLine { key: root.key-open; what: "Open Sweeflow"; }
                    KeyLine { key: root.key-menu; what: "Global menu (from Mac windows)"; }
                    KeyLine { key: root.key-pause; what: "Pause or resume"; }
                    KeyLine { key: root.key-stop; what: "Stop or start"; }
                    if !root.shortcut-settings: Text { text: "Change keys in your Hyprland config."; color: Look.muted; font-size: 11px; }
                }
                // Where Mac shortcuts go: the picture, and under it which keys are which there.
                VerticalLayout {
                    vertical-stretch: 0;
                    padding-top: 16px;
                    spacing: 6px;
                    HorizontalLayout { alignment: start; MacScene { width: 320px; height: 96px; } }
                    KeyMapping { font-size: 12px; swapped: root.swapped; }
                }
                Rectangle {}
                HorizontalLayout {
                    spacing: 8px;
                    Btn { text: "Change shortcuts…"; clicked => { root.change-shortcuts(); } }
                    Rectangle {}
                    Btn { text: "Done"; primary: true; clicked => { root.finish(); } }
                }
            }          }
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
    /// The Mac just linked (its fingerprint), for renaming it on the last page.
    static LINKED: RefCell<String> = const { RefCell::new(String::new()) };
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

    // The window has one size. GNOME keeps to that (with the title bar's double click fixed, see
    // vendor/sctk-adwaita); Hyprland maximises any window it is told to (the button in Omarchy's
    // title bars), so there it is put straight back.
    crate::hyprland::keep_window_size();

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
            ui.set_page(0);
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

    let weak = ui.as_weak();
    ui.on_open_site(move || {
        // In this computer's browser: to read there, or send on to the Mac.
        let Some(ui) = weak.upgrade() else { return };
        let site = ui.get_download_site();
        if let Err(e) = std::process::Command::new("xdg-open").arg(format!("https://{site}")).spawn() {
            log::warn!("opening {site}: {e}");
        }
    });
    ui.set_hyprland(crate::hyprland::running());
    ui.set_omarchy(crate::hyprland::running() && crate::hyprland::omarchy());
    ui.on_finish(|| {
        let _ = slint::quit_event_loop();
    });
    ui.set_shortcut_settings(crate::tray::shortcut_settings_place().is_some());
    // The Shortcuts window: Sweeflow's keys (and the desktop's settings for them), and the keys kept
    // for Linux.
    ui.on_change_shortcuts(|| crate::ui::run_own("shortcuts", "Shortcuts"));
    let weak = ui.as_weak();
    ui.on_rename(move |name| {
        let name = name.trim().to_string();
        let fingerprint = LINKED.with_borrow(|l| l.clone());
        let Some(ui) = weak.upgrade() else { return };
        if name.is_empty() || fingerprint.is_empty() {
            return;
        }
        match rename_mac(&fingerprint, &name) {
            Ok(()) => {
                ui.set_mac_name(name.clone().into());
                ui.set_new_name(name.into());
                ui.set_renamed(true);
            }
            Err(e) => ui.set_error(format!("Couldn't rename it: {e:#}").into()),
        }
    });

    crate::look::fit_to_screen_soon(&ui, |ui, h| ui.set_room(h));
    ui.set_swapped(crate::config::swap_cmd_opt());
    let shown = ui.run();
    // Closed while asking a Mac: say so there (it takes its question down) before leaving.
    if let Some((_, thread)) = cancel_link() {
        let _ = thread.join();
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
    ui.set_page(1);
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
                        // The Mac only links once its setup is done, from its Link window.
                        Outcome::Closed => "Waiting for your Mac. Finish setting up Sweeflow there, or choose Link a Linux machine in its menu. The code then shows up on the Mac.",
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
                            ui.set_page(0);
                            return;
                        }
                    };
                    ui.set_switched_from(before.unwrap_or_default().into());
                    LINKED.with_borrow_mut(|l| *l = p.fingerprint.clone());
                    ui.set_new_name(p.name.clone().into());
                    ui.set_renamed(false);
                    ui.set_mac_name(p.name.into());
                    ui.set_error(SharedString::new());
                    if let Err(e) = after_first_link(&p.fingerprint) {
                        log::warn!("starting to stream from the Mac just linked: {e:#}");
                        ui.set_error(format!("Couldn't start streaming from it: {e:#}").into());
                    }
                    show_keys(&ui);
                    ui.set_page(2);
                }
                Ok(other) => {
                    ui.set_error(other.message(&name).unwrap_or_default().into());
                    ui.set_page(0);
                }
                Err(e) => {
                    log::warn!("linking to {name}: {e:#}");
                    ui.set_error(format!("Linking to {name} didn't work ({e:#}).").into());
                    ui.set_page(0);
                }
            }
        });
    });
    LINKING.with_borrow_mut(|l| *l = Some((cancel, thread)));
}

/// Renames a linked Mac (what this computer calls it): through the running client, which saves
/// it and shows the new name at once, else in the config.
fn rename_mac(fingerprint: &str, name: &str) -> Result<()> {
    let by_client = zbus::blocking::Connection::session().and_then(|bus| {
        bus.call_method(Some("dev.sweeflow.Client"), "/dev/sweeflow/Panel", Some("dev.sweeflow.Panel"), "Rename", &(fingerprint, name))
            .map(|_| ())
    });
    if by_client.is_ok() {
        return Ok(());
    }
    let mut cfg = Config::load_or_default()?;
    if let Some(m) = cfg.macs.iter_mut().find(|m| m.fingerprint == fingerprint) {
        m.name = name.to_string();
        cfg.save()?;
    }
    Ok(())
}

/// The desktop's keys for Sweeflow, read off the UI thread (it asks the desktop) and put on the
/// last page.
fn show_keys(ui: &Wizard) {
    // Sweeflow's keys as set (keys.json), shown again each second: a key changed in the Shortcuts
    // window (opened from here) shows here at once.
    // Only when they changed (setting them redraws the rows).
    let shown = std::cell::RefCell::new(String::new());
    let set = move |ui: &Wizard| {
        let keys = crate::keys::own_json(crate::setup::mac_keys_remapped());
        let now = keys.to_string();
        if *shown.borrow() == now {
            return;
        }
        *shown.borrow_mut() = now;
        let text = |name: &str| crate::keys::caps_model(keys[name].as_str().unwrap_or_default());
        ui.set_key_open(text("open"));
        ui.set_key_pause(text("pause"));
        ui.set_key_stop(text("stop"));
        ui.set_key_menu(text("menu"));
    };
    set(ui);
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), move || {
        if let Some(ui) = weak.upgrade() {
            set(&ui);
        }
    });
    // Lives as long as the window: the event loop ends with it.
    std::mem::forget(timer);
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
        start: String::new(),
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
