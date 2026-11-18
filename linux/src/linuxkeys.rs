//! The Shortcuts window (`sweeflow shortcuts`): Sweeflow's keys (changed in the desktop's own
//! shortcut settings, where they live), and the keys that stay Linux's even in a focused Mac
//! window under the pointer: the built-in ones (deskkeys.rs; Hyprland's in hyprland::mac_keys)
//! and the person's own, added here. Those are typed as text ("Super+E"): the desktop takes keys
//! bound to its shortcuts before any window sees them, so pressing them couldn't record them.
//! Saved at once; the client reads the list again on the next key (deskkeys::is_user_key). A
//! process of its own, like the wizard.

use anyhow::Result;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::deskkeys::{load_exceptions, save_exceptions, Exceptions, UserKey};

slint::slint! {
    import { ScrollView } from "std-widgets.slint";
    import { Look, Label, Dim, Btn, Field, KeyMapping, KeyCaps, Pill } from "look.slint";
    export { Look }

    // A field for a shortcut: pressed in it (a key held with Ctrl, Alt or Super is written out,
    // "Super+E"), or typed. (The desktop keeps keys bound to its own shortcuts: those are typed.)
    component KeyField inherits FocusScope {
        in-out property <string> text <=> field.text;
        in property <string> placeholder;
        // Takes the keyboard as it appears (a Change field).
        in property <bool> autofocus;
        init => {
            if (root.autofocus) {
                field.focus();
            }
        }
        callback accepted(string);
        // The keys of a press, written out ("" when it isn't a shortcut): Rust's KeyEvent words.
        callback combo(string, bool, bool, bool, bool) -> string;
        height: 34px;
        horizontal-stretch: 1;
        forward-focus: field;
        capture-key-pressed(event) => {
            if (event.modifiers.control || event.modifiers.alt || event.modifiers.meta) {
                let written = root.combo(event.text, event.modifiers.meta, event.modifiers.control, event.modifiers.alt, event.modifiers.shift);
                if (written != "") {
                    field.text = written;
                    return accept;
                }
            }
            return reject;
        }
        field := Field { placeholder: root.placeholder; accepted(t) => { root.accepted(t); } }
    }

    // One of Sweeflow's keys: its keycaps, what it does, and Change; while being changed, a
    // field to type the new key in.
    component KeyRow inherits VerticalLayout {
        in property <[string]> key;
        in property <string> what;
        in property <bool> editing;
        in property <bool> mac-style;
        callback change();
        callback save(string);
        callback cancel();
        callback combo(string, bool, bool, bool, bool) -> string;
        spacing: 4px;
        if !root.editing: HorizontalLayout {
            spacing: 8px;
            HorizontalLayout { width: 150px; VerticalLayout { alignment: center; KeyCaps { caps: root.key; } } }
            Label { text: root.what; vertical-alignment: center; horizontal-stretch: 1; }
            VerticalLayout { alignment: center; Btn { text: "Change"; small: true; clicked => { root.change(); } } }
        }
        if root.editing: HorizontalLayout {
            spacing: 8px;
            typed := KeyField {
                placeholder: "Press or type the new shortcut";
                autofocus: true;
                accepted(t) => { root.save(t); }
                combo(t, meta, ctrl, alt, shift) => { return root.combo(t, meta, ctrl, alt, shift); }
            }
            Btn { text: "Save"; primary: true; enabled: typed.text != ""; clicked => { root.save(typed.text); } }
            Btn { text: "Cancel"; clicked => { root.cancel(); } }
        }
    }

    component Heading inherits Text {
        color: Look.fg;
        font-size: 13px;
        font-weight: 700;
    }

    // An on/off switch.
    component Switch inherits Rectangle {
        in-out property <bool> on;
        callback toggled(bool);
        width: 40px;
        height: 22px;
        border-radius: 11px;
        background: root.on ? Look.accent : Look.fg.transparentize(0.75);
        animate background { duration: 150ms; }
        Rectangle {
            width: 16px;
            height: 16px;
            border-radius: 8px;
            y: 3px;
            x: root.on ? parent.width - self.width - 3px : 3px;
            background: root.on ? Look.on-accent : Look.fg;
            animate x { duration: 150ms; }
        }
        TouchArea {
            mouse-cursor: pointer;
            clicked => {
                root.on = !root.on;
                root.toggled(root.on);
            }
        }
    }

    export struct KeptKey { caps: [string], what: string, own: bool, heading: bool, open: bool, count: string }

    export component Shortcuts inherits Window {
        title: "Shortcuts";
        icon: @image-url("../../assets/sweeflow-app.svg");
        background: Look.bg;
        default-font-family: Look.font;
        width: 560px;
        height: 900px;

        in property <[string]> key-open;
        in property <[string]> key-menu;
        in property <[string]> key-pause;
        in property <[string]> key-stop;
        // The desktop's own dialog changes Sweeflow's keys from any window (the portal's,
        // globalkeys.rs); else they're typed here. The Global menu's is always typed here.
        in property <bool> portal;
        // Which key is being typed ("open", "menu", "pause", "stop"; "" none).
        in-out property <string> editing;
        in-out property <string> key-error;
        in property <bool> hyprland;
        in property <bool> omarchy;
        in property <bool> mac-style;
        // In Mac windows, Alt is ⌘ and Super is ⌥ (keyboards made for Windows).
        in-out property <bool> swap-keys;
        callback swap-changed(bool);
        // The keys kept for Linux: the built-in kinds still kept, then the person's own.
        in property <[KeptKey]> keys;
        // Anything taken out or added: Reset to defaults has something to do.
        in property <bool> changed;
        callback reset();
        // A press in a key field, written out (see KeyField).
        pure callback combo(string, bool, bool, bool, bool) -> string;
        // A heading in the kept keys was clicked: its keys shown or hidden.
        callback toggle(int);
        in-out property <string> error;
        callback change-key(string);
        callback save-key(string, string);
        callback add(string);
        callback remove(int);
        callback done();

        VerticalLayout {
            padding: 24px;
            spacing: 16px;
            // As the app's other windows open: its logo and the title.
            HorizontalLayout {
                spacing: 12px;
                alignment: start;
                Image { source: @image-url("../../assets/sweeflow-app.svg"); width: 40px; height: 40px; }
                VerticalLayout { alignment: center; Text { text: "Shortcuts"; color: Look.fg; font-size: 20px; font-weight: 600; } }
            }
            // What Sweeflow does with the keyboard, up front: as if plugged into the Mac, with the
            // modifiers as they reach it (following the swap below).
            VerticalLayout {
                spacing: 4px;
                Label { text: "Sweeflow maps your keyboard to Mac windows as if it were plugged into the Mac."; }
                Label { text: root.swap-keys ? "In Mac windows Alt is ⌘, Super is ⌥." : "In Mac windows Super is ⌘, Alt is ⌥."; }
            }
            // The swap for keyboards made for Windows, on a card: ⌘ next to the space bar in Mac
            // windows, as on a Mac keyboard.
            Rectangle {
                border-radius: Look.radius + 2px;
                border-width: Look.outlined ? 1px : 0px;
                border-color: Look.line;
                background: Look.surface;
                height: swap-row.preferred-height;
                swap-row := HorizontalLayout {
                    padding: 14px;
                    spacing: 12px;
                    VerticalLayout {
                        spacing: 3px;
                        horizontal-stretch: 1;
                        Heading { text: "Swap ⌘ and ⌥ for PC keyboards"; }
                        Dim { text: "For those used to an Apple keyboard, it puts ⌘ next to the space bar."; font-size: 12px; }
                    }
                    VerticalLayout { alignment: center; Switch { on <=> root.swap-keys; toggled(on) => { root.swap-changed(on); } } }
                }
            }

            // Sweeflow's keys, on a card.
            Rectangle {
                border-radius: Look.radius + 2px;
                border-width: Look.outlined ? 1px : 0px;
                border-color: Look.line;
                background: Look.surface;
                height: own-keys.preferred-height;
                own-keys := VerticalLayout {
                    padding: 14px;
                    spacing: 8px;
                    Heading { text: "Sweeflow shortcuts"; }
                    for k in [
                        { name: "open", key: root.key-open, what: "Open Sweeflow" },
                        { name: "menu", key: root.key-menu, what: "Global menu (from Mac windows)" },
                        { name: "pause", key: root.key-pause, what: "Pause or resume" },
                        { name: "stop", key: root.key-stop, what: "Stop or start" },
                    ]: KeyRow {
                        key: k.key;
                        what: k.what;
                        mac-style: root.mac-style;
                        editing: root.editing == k.name;
                        change => { root.key-error = ""; root.change-key(k.name); }
                        save(t) => { root.save-key(k.name, t); }
                        combo(t, meta, ctrl, alt, shift) => { return root.combo(t, meta, ctrl, alt, shift); }
                        cancel => { root.editing = ""; root.key-error = ""; }
                    }
                    if root.key-error != "": Label { text: root.key-error; color: Look.danger; }
                }
            }

            // The keys that stay Linux's, on a card: sections that open, then a key to add.
            Rectangle {
                vertical-stretch: 1;
                border-radius: Look.radius + 2px;
                border-width: Look.outlined ? 1px : 0px;
                border-color: Look.line;
                background: Look.surface;
                VerticalLayout {
                    padding: 14px;
                    spacing: 10px;
                    VerticalLayout {
                        spacing: 3px;
                        Heading { text: "Shortcuts kept for Linux"; }
                        Dim { text: "A superset of typical Linux shortcuts, excluded from your Mac shortcuts. Remove defaults or add new ones."; font-size: 12px; }
                    }
                    if root.keys.length == 0: Dim { text: "None: every shortcut goes to the Mac."; vertical-stretch: 1; vertical-alignment: center; horizontal-alignment: center; }
                    if root.keys.length > 0: ScrollView {
                        vertical-stretch: 1;
                        VerticalLayout {
                            spacing: 2px;
                            alignment: start;
                            for k[i] in root.keys: Rectangle {
                                // Two lines for a long description.
                                height: k.heading ? 36px : (k.what.character-count > 34 ? 46px : 34px);
                                border-radius: Look.radius;
                                background: k.heading && heading-touch.has-hover ? Look.fg.transparentize(0.94) : transparent;
                                // A heading: the whole row opens or closes its section.
                                heading-touch := TouchArea {
                                    enabled: k.heading;
                                    mouse-cursor: k.heading ? pointer : default;
                                    clicked => { root.toggle(i); }
                                }
                                if k.heading: HorizontalLayout {
                                    padding-left: 6px;
                                    padding-right: 6px;
                                    spacing: 8px;
                                    Text { text: k.open ? "▾" : "▸"; color: Look.fg; font-size: 13px; vertical-alignment: center; }
                                    Text { text: k.what; color: Look.fg; font-size: 13px; font-weight: 600; vertical-alignment: center; }
                                    Text { text: k.count; color: Look.muted; font-size: 12px; vertical-alignment: center; }
                                    Rectangle {}
                                }
                                if !k.heading: HorizontalLayout {
                                    y: 0;
                                    height: parent.height;
                                    padding-left: 26px;
                                    // Clear of the scroll bar.
                                    padding-right: 12px;
                                    spacing: 10px;
                                    HorizontalLayout { width: 150px; VerticalLayout { alignment: center; KeyCaps { caps: k.caps; size: 12px; } } }
                                    Text { text: k.what; color: Look.fg; font-size: 13px; vertical-alignment: center; wrap: word-wrap; horizontal-stretch: 1; }
                                    VerticalLayout { alignment: center; Pill { text: "Remove"; icon: "M 6 6 L 18 18 M 18 6 L 6 18"; clicked => { root.remove(i); } } }
                                }
                            }
                        }
                    }
                    HorizontalLayout {
                        spacing: 8px;
                        field := KeyField {
                            placeholder: "Add a shortcut";
                            accepted(t) => { root.add(t); field.text = ""; }
                            combo(t, meta, ctrl, alt, shift) => { return root.combo(t, meta, ctrl, alt, shift); }
                        }
                        Btn { text: "Add"; enabled: field.text != ""; clicked => { root.add(field.text); field.text = ""; } }
                    }
                    if root.error != "": Label { text: root.error; color: Look.danger; }
                    if root.hyprland: Dim { text: "On Hyprland, only shortcuts that start a program can be kept."; font-size: 12px; }
                }
            }
            HorizontalLayout {
                spacing: 8px;
                Btn { text: "Reset to defaults"; enabled: root.changed; clicked => { root.reset(); } }
                Rectangle {}
                Btn { text: "Done"; primary: true; clicked => { root.done(); } }
            }
        }
    }
}

pub fn run() -> Result<()> {
    let Some(_only) = crate::lock("sweeflow-shortcuts.lock")? else {
        log::info!("the Shortcuts window is already open");
        return Ok(());
    };
    let ui = Shortcuts::new()?;
    crate::look::apply!(ui);
    if let Err(e) = slint::set_xdg_app_id("sweeflow") {
        log::warn!("app id: {e}");
    }
    crate::hyprland::keep_window_size();
    let hyprland = crate::hyprland::running();
    ui.set_hyprland(hyprland);
    ui.set_omarchy(hyprland && crate::hyprland::omarchy());
    ui.set_swap_keys(crate::config::swap_cmd_opt());
    ui.on_swap_changed(|on| {
        if let Err(e) = crate::config::set_swap_cmd_opt(on) {
            log::warn!("saving the Alt and Super swap: {e:#}");
        }
    });
    ui.set_portal(crate::globalkeys::in_use() && !crate::hyprland::running());
    // Sweeflow's keys as set (keys.json), shown again whenever they change: here, or in the
    // desktop's dialog (the client saves what it gives back).
    let show_own = {
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let keys = crate::keys::own_json(crate::setup::mac_keys_remapped());
            let text = |name: &str| crate::keys::caps_model(keys[name].as_str().unwrap_or_default());
            ui.set_key_open(text("open"));
            ui.set_key_menu(text("menu"));
            ui.set_key_pause(text("pause"));
            ui.set_key_stop(text("stop"));
        }
    };
    show_own();
    let refresh = slint::Timer::default();
    refresh.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), show_own.clone());
    let weak = ui.as_weak();
    ui.on_change_key(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_portal() && name != "menu" {
            tell_client("configure-keys", "");
        } else {
            ui.set_editing(name);
        }
    });
    let weak = ui.as_weak();
    let shown_again = show_own.clone();
    ui.on_save_key(move |name, text| {
        let Some(ui) = weak.upgrade() else { return };
        match UserKey::parse(&text) {
            Some(k) if k.logo || k.alt || k.ctrl || k.key.starts_with('F') && k.key.len() > 1 => {
                match crate::keys::set_own_key(&name, &k.label()) {
                    Ok(()) => {
                        ui.set_editing(SharedString::new());
                        ui.set_key_error(SharedString::new());
                        shown_again();
                        tell_client("keys-changed", &name);
                    }
                    Err(e) => ui.set_key_error(format!("Couldn't save: {e}").into()),
                }
            }
            Some(k) => ui.set_key_error(format!("{} on its own is typing: add Super, Ctrl or Alt to it.", k.label()).into()),
            None => ui.set_key_error(format!("“{text}” isn't a shortcut. Write it like Super+Alt+O.").into()),
        }
    });

    // The keys kept for Linux: each built-in kind still kept (as this desktop has it), then the
    // person's own ("Super+E" as saved; the Mac's way behind a remapper, ⌘E: the ⌘ key gives
    // Super in Mac windows there). Rows and what each one is, for taking it out.
    // Keys as the keyboard names them (Super+Tab), a remapper or not; behind one a press is turned
    // back into those names (below).
    let mac_style = false;
    let remapped = crate::setup::mac_keys_remapped();
    // Asked of the desktop once (it takes a moment): every key it keeps over Mac windows.
    let all = std::rc::Rc::new(crate::keptkeys::all());
    let rows: std::rc::Rc<std::cell::RefCell<Vec<Row>>> = Default::default();
    let model = std::rc::Rc::new(VecModel::<KeptKey>::default());
    ui.set_keys(ModelRc::from(model.clone()));
    // The headings open (their keys shown): only "Your own" at first, so the list starts short.
    let open: std::rc::Rc<std::cell::RefCell<Vec<String>>> = std::rc::Rc::new(std::cell::RefCell::new(vec!["Your own".to_string()]));
    let show = {
        let (rows, model, weak, all, open) = (rows.clone(), model.clone(), ui.as_weak(), all.clone(), open.clone());
        move |e: &Exceptions| {
            let mut list = Vec::new();
            let mut shown = Vec::new();
            let caps = |label: &str| crate::keys::caps_model(&UserKey::parse(label).map_or(label.to_string(), |k| if mac_style { k.mac_label() } else { k.label() }));
            let open = open.borrow();
            let heading = |name: &str, count: usize, list: &mut Vec<Row>, shown: &mut Vec<KeptKey>| {
                list.push(Row::Heading(name.to_string()));
                shown.push(KeptKey {
                    caps: ModelRc::default(),
                    what: name.into(),
                    own: false,
                    heading: true,
                    open: open.iter().any(|o| o == name),
                    count: count.to_string().into(),
                });
            };
            for group in crate::keptkeys::GROUPS {
                let keys: Vec<_> = all.iter().filter(|k| k.group == group && !e.removed.contains(&k.label)).collect();
                if keys.is_empty() {
                    continue;
                }
                heading(group, keys.len(), &mut list, &mut shown);
                if !open.iter().any(|o| o == group) {
                    continue;
                }
                for k in keys {
                    list.push(Row::Kind(k.label.clone()));
                    shown.push(KeptKey { caps: caps(&k.label), what: k.what.clone().into(), own: false, heading: false, open: false, count: SharedString::new() });
                }
            }
            if !e.added.is_empty() {
                heading("Your own", e.added.len(), &mut list, &mut shown);
                if open.iter().any(|o| o == "Your own") {
                    for k in &e.added {
                        let label = UserKey::parse(k).map_or(k.clone(), |k| if mac_style { k.mac_label() } else { k.label() });
                        list.push(Row::Own(k.clone()));
                        shown.push(KeptKey { caps: crate::keys::caps_model(&label), what: SharedString::new(), own: true, heading: false, open: false, count: SharedString::new() });
                    }
                }
            }
            *rows.borrow_mut() = list;
            model.set_vec(shown);
            if let Some(ui) = weak.upgrade() {
                ui.set_changed(*e != Exceptions::default());
            }
        }
    };
    show(&load_exceptions());
    let store = {
        let show = show.clone();
        move |ui: &Shortcuts, e: &Exceptions| match save_exceptions(e) {
            Ok(()) => {
                log::info!("keys kept for Linux: {e:?}");
                show(e);
                // Hyprland's key mode is made again with them.
                tell_client("exceptions-changed", "");
            }
            Err(err) => ui.set_error(format!("Couldn't save: {err}").into()),
        }
    };

    let weak = ui.as_weak();
    let save = store.clone();
    ui.on_add(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        match UserKey::parse(&text) {
            Some(k) if k.logo || k.alt || k.ctrl || k.key.len() > 1 => {
                let mut e = load_exceptions();
                let label = k.label();
                if e.added.contains(&label) {
                    ui.set_error(format!("{} is there already.", if mac_style { k.mac_label() } else { label }).into());
                    return;
                }
                e.added.push(label);
                ui.set_error(SharedString::new());
                save(&ui, &e);
            }
            // A plain letter would never reach the Mac again.
            Some(k) => ui.set_error(format!("{} on its own is typing: add Super, Ctrl or Alt to it.", k.label()).into()),
            None => ui.set_error(format!("“{text}” isn't a shortcut. Write it like Super+E or Ctrl+Alt+T.").into()),
        }
    });
    let weak = ui.as_weak();
    let save = store.clone();
    let removed_rows = rows.clone();
    ui.on_remove(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(row) = removed_rows.borrow().get(i as usize).cloned() else { return };
        let mut e = load_exceptions();
        match row {
            Row::Kind(label) => e.removed.push(label),
            Row::Own(k) => e.added.retain(|a| *a != k),
            Row::Heading(_) => return,
        }
        save(&ui, &e);
    });
    let weak = ui.as_weak();
    ui.on_reset(move || {
        if let Some(ui) = weak.upgrade() {
            store(&ui, &Exceptions::default());
        }
    });
    // A heading clicked: its keys shown or hidden.
    let (show_again, toggled_rows) = (show.clone(), rows.clone());
    ui.on_toggle(move |i| {
        let Some(Row::Heading(name)) = toggled_rows.borrow().get(i as usize).cloned() else { return };
        {
            let mut open = open.borrow_mut();
            match open.iter().position(|o| *o == name) {
                Some(at) => {
                    open.remove(at);
                }
                None => open.push(name),
            }
        }
        show_again(&load_exceptions());
    });
    // Behind a Mac-style remapper, the key in ⌘'s place reaches Linux windows as Ctrl and the
    // one in ⌃'s place as Super: a press is written the Mac way (⌘ as Super, as Mac windows get it).
    ui.on_combo(move |text, meta, ctrl, alt, shift| {
        let (meta, ctrl) = if remapped { (ctrl, meta) } else { (meta, ctrl) };
        let written = combo(&text, meta, ctrl, alt, shift);
        // Shown the way the window shows keys: ⌘⌥E behind a remapper (read back the same).
        match UserKey::parse(&written) {
            Some(k) if mac_style => k.mac_label().into(),
            _ => written.into(),
        }
    });
    ui.on_done(|| {
        let _ = slint::quit_event_loop();
    });
    ui.run()?;
    Ok(())
}

/// Tells the running client (its D-Bus panel interface, panel.rs) to do something.
fn tell_client(action: &str, arg: &str) {
    let sent = zbus::blocking::Connection::session().and_then(|bus| {
        bus.call_method(Some("dev.sweeflow.Client"), "/dev/sweeflow/Panel", Some("dev.sweeflow.Panel"), "Do", &(action, arg)).map(|_| ())
    });
    if let Err(e) = sent {
        log::warn!("{action}: Sweeflow isn't running ({e})");
    }
}

/// A row of the kept keys: a built-in kind (deskkeys::KINDS) or one of the person's own.
#[derive(Clone)]
enum Row {
    Heading(String),
    Kind(String),
    Own(String),
}

/// A press in a key field written out ("Super+E", "Ctrl+Alt+F5"), from Slint's key event: its
/// text (a character, or one of Slint's codes for named keys) and modifiers. "" for a press
/// that's only a modifier, or no key Sweeflow can name.
fn combo(text: &str, meta: bool, ctrl: bool, alt: bool, shift: bool) -> String {
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else { return String::new() };
    let key = match c {
        '\t' | '\u{19}' => "Tab".to_string(),
        '\n' | '\r' => "Enter".into(),
        '\u{1b}' => "Esc".into(),
        '\u{7f}' => "Delete".into(),
        '\u{8}' => "Backspace".into(),
        ' ' => "Space".into(),
        '\u{F700}' => "Up".into(),
        '\u{F701}' => "Down".into(),
        '\u{F702}' => "Left".into(),
        '\u{F703}' => "Right".into(),
        '\u{F729}' => "Home".into(),
        '\u{F72B}' => "End".into(),
        '\u{F72C}' => "PageUp".into(),
        '\u{F72D}' => "PageDown".into(),
        '\u{F727}' => "Insert".into(),
        f @ '\u{F704}'..='\u{F71B}' => format!("F{}", f as u32 - 0xF704 + 1),
        // Shift's signs on the digits (a US layout): the digit, Shift is held anyway.
        s if shift && ")!@#$%^&*(".contains(s) => ")!@#$%^&*(".find(s).unwrap().to_string(),
        c if c.is_control() || ('\u{F000}'..='\u{F8FF}').contains(&c) => return String::new(),
        c => c.to_uppercase().collect(),
    };
    let mods = [(meta, "Super"), (ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift")];
    let mut parts: Vec<&str> = mods.iter().filter(|m| m.0).map(|m| m.1).collect();
    parts.push(&key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    #[test]
    fn presses_are_written_out() {
        assert_eq!(super::combo("e", true, false, false, false), "Super+E");
        assert_eq!(super::combo("\u{F708}", false, true, true, false), "Ctrl+Alt+F5");
        assert_eq!(super::combo("!", true, false, false, true), "Super+Shift+1");
        assert_eq!(super::combo("\t", false, false, true, false), "Alt+Tab");
        // A modifier on its own (Slint's code for it): nothing yet.
        assert_eq!(super::combo("\u{0010}", true, false, false, false), "");
    }
}
