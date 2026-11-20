//! Sweeflow's own window (`sweeflow panel`), for desktops without the Plasma widget (GNOME,
//! Hyprland): a tray menu there is drawn by the desktop and closes after every click, so ticking
//! several things meant reopening it each time, and it can't show running apps, pin or quit them,
//! or rename a Mac. A process of its own, like the pairing wizard, that talks to the running
//! client over D-Bus exactly as the widget does (the `State` property and its change signal,
//! `Set`, `Do`, `Rename`; see panel.rs), so the client keeps every bit of the logic.
//!
//! It closes on Esc, its own close button, or losing focus (a click elsewhere); nothing chosen
//! inside closes it. Nothing in it depends on the desktop it runs on.

use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::mpsc};

use anyhow::Result;
use serde::Deserialize;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

slint::slint! {
    import { ScrollView } from "std-widgets.slint";
    import { Look, Label, Dim, Btn, Field, Sign, Tip, TipBubble, KeyCaps } from "look.slint";
    export { Look }

    export struct AppRow { id: string, name: string, icon: image, has-icon: bool, initial: string, running: bool, pinned: bool, shown: bool, heading: string }
    export struct MacItem { name: string, fingerprint: string, active: bool, online: string, via: string }
    // tip: why it can't be chosen now (it's then shown faded, and says so on hover); empty: it can.
    export struct Action { id: string, label: string, tip: string }
    // tip: what it does, on hover (the Plasma widget's hint).
    export struct Toggle { key: string, label: string, on: bool, tip: string, note: string }

    // A small button, for the actions on a row.
    component Chip inherits Rectangle {
        in property <string> text;
        in property <bool> enabled: true;
        in property <bool> danger;
        // A drawn sign instead of the text (see Sign); the text stays as the accessible name.
        in property <string> sign;
        // Shown when the pointer rests on it.
        in property <string> tip;
        callback clicked;
        height: 26px;
        width: root.sign != "" ? 34px : max(44px, label.preferred-width + 18px);
        opacity: root.enabled ? 1 : 0.45;
        border-radius: Look.radius;
        // As every button (look.slint's Btn): violet under the pointer.
        border-width: 1px;
        border-color: root.danger ? Look.danger : Look.button-line(touch.has-hover && root.enabled);
        background: root.danger ? Look.danger.transparentize(touch.has-hover && root.enabled ? 0.7 : 0.82)
            : Look.button-fill(touch.has-hover && root.enabled, touch.pressed);
        accessible-role: button;
        accessible-label: root.text;
        accessible-action-default => { if (root.enabled) { root.clicked(); } }
        // Enabled even when the chip isn't, for the hover tip.
        touch := TouchArea {
            // Enabled even when the chip isn't, for the hover tip (why it can't be chosen).
            mouse-cursor: root.enabled ? pointer : default;
            clicked => { Tip.leave(); if (root.enabled) { root.clicked(); } }
            changed has-hover => {
                if (self.has-hover && root.tip != "") { Tip.enter(root.tip, root.absolute-position.x, root.absolute-position.y, root.width, root.height); } else { Tip.leave(); }
            }
        }
        label := Text {
            visible: root.sign == "";
            text: root.text;
            color: root.danger ? Look.danger : Look.fg;
            font-size: 12px;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
        if root.sign != "": Sign {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            kind: root.sign;
        }
    }

    // On or off; it stays as clicked (the client confirms a moment later).
    component Switch inherits Rectangle {
        in property <bool> on;
        in property <bool> enabled: true;
        in property <string> tip;
        callback toggled;
        width: 36px;
        height: 20px;
        opacity: root.enabled ? 1 : 0.45;
        border-radius: Look.outlined ? min(Look.radius, 10px) : 10px;
        border-width: Look.outlined ? 1px : 0px;
        border-color: root.on ? Look.accent : Look.line;
        background: root.on ? Look.accent : Look.fg.transparentize(0.8);
        accessible-role: checkbox;
        accessible-checkable: true;
        accessible-checked: root.on;
        accessible-action-default => { if (root.enabled) { root.toggled(); } }
        Rectangle {
            width: 14px;
            height: 14px;
            y: 3px;
            x: root.on ? parent.width - 17px : 3px;
            border-radius: Look.outlined ? min(Look.radius, 7px) : 7px;
            background: root.on ? Look.on-accent : Look.fg;
            animate x { duration: 120ms; }
        }
        TouchArea {
            enabled: root.enabled;
            mouse-cursor: pointer;
            clicked => { root.toggled(); }
            changed has-hover => {
                if (self.has-hover && root.tip != "") { Tip.enter(root.tip, root.absolute-position.x, root.absolute-position.y, root.width, root.height); } else { Tip.leave(); }
            }
        }
    }

    // Where a Mac stands (MacItem.online), as its dot: Streaming violet and Paused a violet ring
    // (streaming to this computer); Available green; Busy or Locked orange (in the way); Not
    // online a grey ring.
    global MacState {
        pure public function ink(state: string) -> color {
            state == "streaming" || state == "paused" ? Look.accent
                : state == "available" ? #30a46c
                : state == "busy" || state == "locked" ? #e8890c
                : Look.muted
        }
        pure public function hollow(state: string) -> bool { state == "paused" || state == "offline" }
        pure public function words(state: string) -> string {
            state == "streaming" ? "Streaming" : state == "paused" ? "Paused" : state == "available" ? "Available"
                : state == "busy" ? "Streaming elsewhere" : state == "locked" ? "Locked" : "Not online"
        }
    }

    // How a Mac is reached, after its status: "Local: 192.168.1.20", "Remote: 100.64.0.3",
    // "VM: 127.0.0.1".
    component Via inherits Text {
        color: Look.muted;
        font-size: 11px;
        overflow: elide;
        vertical-alignment: center;
    }

    component Tab inherits Rectangle {
        in property <string> text;
        in property <bool> selected;
        callback clicked;
        height: 32px;
        horizontal-stretch: 1;
        accessible-role: tab;
        accessible-label: root.text;
        accessible-action-default => { root.clicked(); }
        TouchArea {
            mouse-cursor: pointer;
            clicked => { Tip.leave(); root.clicked(); }
        }
        Text {
            text: root.text;
            color: root.selected ? Look.fg : Look.muted;
            font-size: 13px;
            font-weight: root.selected ? 600 : 400;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
        Rectangle {
            y: parent.height - 2px;
            height: 2px;
            background: root.selected ? Look.accent : Look.line.transparentize(0.5);
        }
    }

    // A row that does something when clicked anywhere on it, with room on the right.
    component Row inherits Rectangle {
        in property <bool> clickable: true;
        callback clicked;
        border-radius: Look.radius;
        background: touch.has-hover && root.clickable ? Look.fg.transparentize(0.93) : transparent;
        touch := TouchArea {
            enabled: root.clickable;
            mouse-cursor: pointer;
            clicked => { root.clicked(); }
        }
        @children
    }

    export component PanelWin inherits Window {
        title: "Sweeflow";
        icon: @image-url("../../assets/sweeflow-app.svg");
        background: Look.bg;
        default-font-family: Look.font;
        // A fixed size: a dialog, which tiling window managers leave floating.
        width: 420px;
        // Less on a short screen (see look::fit_to_screen): the tabs scroll.
        in property <length> room: 600px;
        height: min(600px, room);
        forward-focus: keys;

        // The client answers on D-Bus (it isn't running otherwise).
        in property <bool> running;
        // The desktop has a place to change keyboard shortcuts (Plasma, GNOME; not Hyprland).
        in property <bool> shortcut-settings;
        // The keys the desktop has now for opening Sweeflow, pausing and stopping ("Super+Alt+P", or as
        // the desktop writes them; "" for none).
        in property <[string]> key-open;
        in property <[string]> key-pause;
        in property <[string]> key-stop;
        in property <[string]> key-menu;
        in property <bool> paired;
        in property <string> mac-name;
        in property <string> status;
        // Where the Mac in use stands (as MacItem.online): the status line takes its colour.
        in property <string> status-state;
        // How the Mac in use is reached, in a few words (as MacItem.via).
        in property <string> status-via;
        // What can be done about the Mac in use (Pause and Stop, Resume and Stop, Start, or Stop).
        in property <[Action]> actions;
        in property <[AppRow]> apps;
        in property <int> app-count;
        in-out property <string> search-text;
        in property <[Toggle]> toggles;
        in property <bool> dim-enabled;
        in property <bool> dimmed;
        in property <bool> display-shown;
        in property <[MacItem]> macs;
        in-out property <int> tab: 0;
        // The keyboard shortcuts' popover is showing (the ⌘ button in the header).
        in-out property <bool> show-keys: false;
        // Hyprland (and Omarchy on it): its own keys stay first over Mac windows.
        in property <bool> hyprland;
        in property <bool> omarchy;
        // A Mac-style key remapper (Toshy): keys written the Mac's way, no mapping line.
        in property <bool> mac-style;
        // Alt and Super swapped in Mac windows (config::swap_cmd_opt).
        in property <bool> swapped;
        // A newer Sweeflow: its version ("" for none), how it's updated (update.rs How: "self",
        // "system", "download"), that in words, and its release page.
        in property <string> update-version;
        in property <string> update-how;
        in property <string> update-words;
        in property <string> update-page;
        // The Mac on another version of Sweeflow, in words ("" when the same).
        in property <string> mac-note;
        // The Mac being renamed, or asked about forgetting (fingerprints).
        in-out property <string> editing;
        in-out property <string> confirming;

        callback search(string);
        callback act(string, string);
        callback flip(string);
        callback dim();
        callback pin(string, bool);
        callback show-app(string, bool);
        callback rename(string, string);
        callback start-client();
        callback quit-all();
        callback dismiss();

        changed search-text => { root.search(self.search-text); }

        keys := FocusScope {
            // Before any field: Esc puts back a rename or a question first, then closes.
            capture-key-pressed(event) => {
                if (event.text == Key.Escape) {
                    if (root.editing != "" || root.confirming != "") {
                        root.editing = "";
                        root.confirming = "";
                    } else {
                        root.dismiss();
                    }
                    return accept;
                }
                return reject;
            }

            VerticalLayout {
                padding: 16px;
                spacing: 12px;

                // The Mac, where streaming stands, and what to do about it.
                HorizontalLayout {
                    spacing: 12px;
                    vertical-stretch: 0;
                    Image { source: @image-url("../../assets/sweeflow-app.svg"); width: 40px; height: 40px; }
                    VerticalLayout {
                        alignment: center;
                        spacing: 3px;
                        horizontal-stretch: 1;
                        Text {
                            text: root.running && root.paired ? root.mac-name : "Sweeflow";
                            color: Look.fg;
                            font-size: 15px;
                            font-weight: 600;
                            overflow: elide;
                        }
                        if root.running && root.paired: HorizontalLayout {
                            spacing: 6px;
                            VerticalLayout {
                                alignment: center;
                                // The Mac in use's dot, as in the list of Macs.
                                Rectangle {
                                    width: 8px;
                                    height: 8px;
                                    border-radius: 4px;
                                    background: root.status-state == "" || MacState.hollow(root.status-state) ? transparent : MacState.ink(root.status-state);
                                    border-width: root.status-state == "" || MacState.hollow(root.status-state) ? 2px : 0px;
                                    border-color: root.status-state == "" ? Look.muted : MacState.ink(root.status-state);
                                }
                            }
                            Text { text: root.status; color: Look.muted; font-size: 12px; overflow: elide; horizontal-stretch: 0; }
                            if root.status-via != "": Via { text: "· " + root.status-via; font-size: 12px; horizontal-stretch: 1; }
                        }
                    }
                    // What can be done in this state (pause and stop, resume and stop, or start),
                    // as signs at the right of the name: a row of their own took room from the list.
                    if root.running && root.paired: HorizontalLayout {
                        spacing: 6px;
                        horizontal-stretch: 0;
                        for a in root.actions: VerticalLayout {
                            alignment: center;
                            Chip { text: a.label; sign: a.id; enabled: a.tip == ""; tip: a.tip != "" ? a.tip : Tip.stream(a.id); clicked => { root.act(a.id, ""); } }
                        }
                    }
                }

                // A newer Sweeflow (the update window says which and how to get it).
                if root.update-version != "": Rectangle {
                    vertical-stretch: 0;
                    border-radius: 8px;
                    background: Look.accent.transparentize(0.88);
                    HorizontalLayout {
                        padding: 10px;
                        spacing: 8px;
                        VerticalLayout {
                            alignment: center;
                            horizontal-stretch: 1;
                            spacing: 2px;
                            Text { text: "Update available"; color: Look.fg; font-size: 13px; font-weight: 600; overflow: elide; }
                        }
                        // The update window: what's new, and the update (or how it's done).
                        VerticalLayout {
                            alignment: center;
                            Btn { text: root.update-how == "self" ? "Update" : "What's New"; small: true; primary: true; clicked => { root.act("update-window", ""); } }
                        }
                    }
                }
                // The Mac on another version.
                if root.mac-note != "": Text { text: root.mac-note; color: Look.muted; font-size: 11px; wrap: word-wrap; vertical-stretch: 0; }

                if root.running && root.paired: HorizontalLayout {
                    vertical-stretch: 0;
                    Tab { text: root.app-count > 0 ? "Apps (" + root.app-count + ")" : "Apps"; selected: root.tab == 0; clicked => { root.tab = 0; } }
                    Tab { text: "Settings"; selected: root.tab == 1; clicked => { root.tab = 1; } }
                    Tab { text: "Macs"; selected: root.tab == 2; clicked => { root.tab = 2; } }
                }

                // Apps: pinned, running, the rest (the client sorts them so), under headings.
                if root.running && root.paired && root.tab == 0: VerticalLayout {
                    spacing: 8px;
                    vertical-stretch: 1;
                    Field {
                        placeholder: "Search apps";
                        text <=> root.search-text;
                        // Ready to type into whenever the tab opens.
                        init => { self.focus(); }
                    }
                    Rectangle {
                        vertical-stretch: 1;
                        if root.apps.length == 0: Dim {
                            text: root.search-text != "" ? "No matching apps" : "The Mac's apps show here once it has connected.";
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                        if root.apps.length > 0: ScrollView {
                            VerticalLayout {
                                alignment: start;
                                padding-right: 12px;
                                spacing: 2px;
                                for a in root.apps: VerticalLayout {
                                    if a.heading != "": Text {
                                        text: a.heading;
                                        color: Look.muted;
                                        font-size: 11px;
                                        font-weight: 600;
                                        height: 26px;
                                        vertical-alignment: bottom;
                                    }
                                    // Click to open it; the switch says whether its windows come here.
                                    Row {
                                        height: 40px;
                                        clicked => { root.act("launch", a.id); }
                                        HorizontalLayout {
                                            padding-left: 6px;
                                            padding-right: 4px;
                                            spacing: 8px;
                                            VerticalLayout {
                                                alignment: center;
                                                if a.has-icon: Image { source: a.icon; width: 28px; height: 28px; }
                                                if !a.has-icon: Rectangle {
                                                    width: 28px;
                                                    height: 28px;
                                                    border-radius: min(Look.radius, 7px);
                                                    background: Look.surface;
                                                    Text { text: a.initial; color: Look.muted; font-size: 13px; font-weight: 600; horizontal-alignment: center; vertical-alignment: center; }
                                                }
                                            }
                                            Text {
                                                text: a.name;
                                                color: Look.fg;
                                                font-size: 13px;
                                                vertical-alignment: center;
                                                overflow: elide;
                                                horizontal-stretch: 1;
                                                min-width: 40px;
                                            }
                                            // Running on the Mac (a pinned one shows it here too).
                                            if a.running: VerticalLayout {
                                                alignment: center;
                                                Rectangle { width: 6px; height: 6px; border-radius: 3px; background: Look.accent; }
                                            }
                                            if a.running: VerticalLayout {
                                                alignment: center;
                                                Chip { text: "Quit"; tip: "Quit this app on the Mac"; clicked => { root.act("quit-app", a.id); } }
                                            }
                                            VerticalLayout {
                                                alignment: center;
                                                Chip { text: a.pinned ? "Unpin" : "Pin"; tip: a.pinned ? "Unpin from the top" : "Pin to the top"; clicked => { root.pin(a.id, !a.pinned); } }
                                            }
                                            VerticalLayout {
                                                alignment: center;
                                                Switch { on: a.shown; tip: a.shown ? "Block this app from streaming" : "Allow this app to stream"; toggled => { root.show-app(a.id, !a.shown); } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Settings, every one a switch.
                if root.running && root.paired && root.tab == 1: ScrollView {
                    vertical-stretch: 1;
                    VerticalLayout {
                        alignment: start;
                        padding-right: 12px;
                        spacing: 2px;
                        for t in root.toggles: Row {
                            height: 40px;
                            clicked => { root.flip(t.key); }
                            HorizontalLayout {
                                padding-left: 8px;
                                padding-right: 6px;
                                spacing: 8px;
                                VerticalLayout {
                                    alignment: center;
                                    horizontal-stretch: 1;
                                    Text { text: t.label; color: Look.fg; font-size: 13px; overflow: elide; }
                                    // A setting waiting on something (the Mac's password).
                                    if t.note != "": Text { text: t.note; color: Look.muted; font-size: 11px; overflow: elide; }
                                }
                                VerticalLayout {
                                    alignment: center;
                                    Switch { on: t.on; tip: t.tip; toggled => { root.flip(t.key); } }
                                }
                            }
                        }
                        Rectangle { height: 9px; Rectangle { height: 1px; background: Look.line.transparentize(0.4); } }
                        // Dims the Mac's own screen now; using the Mac itself brings it back.
                        HorizontalLayout {
                            padding-left: 8px;
                            padding-right: 6px;
                            spacing: 8px;
                            height: 48px;
                            VerticalLayout {
                                alignment: center;
                                horizontal-stretch: 1;
                                spacing: 2px;
                                Text { text: "Dim Mac screen"; color: Look.fg; font-size: 13px; }
                                Text { text: "Until someone uses the Mac itself"; color: Look.muted; font-size: 11px; overflow: elide; }
                            }
                            VerticalLayout {
                                alignment: center;
                                Chip { text: root.dimmed ? "Dimmed" : "Dim"; enabled: root.dim-enabled; clicked => { root.dim(); } }
                            }
                        }
                        // The Mac's whole screen in a window, to see what's going on there.
                        HorizontalLayout {
                            padding-left: 8px;
                            padding-right: 6px;
                            spacing: 8px;
                            height: 48px;
                            VerticalLayout {
                                alignment: center;
                                horizontal-stretch: 1;
                                spacing: 2px;
                                Text { text: "Troubleshoot (stream display)"; color: Look.fg; font-size: 13px; }
                                Text { text: "Show the Mac's whole screen in a window"; color: Look.muted; font-size: 11px; overflow: elide; }
                            }
                            VerticalLayout {
                                alignment: center;
                                Chip { text: root.display-shown ? "Hide" : "Show"; enabled: root.dim-enabled || root.dimmed || root.display-shown; clicked => { root.act("display", ""); } }
                            }
                        }
                    }
                }

                // The linked Macs: each one's name, Online / Not online, its stream actions (those of
                // its state for the Mac in use; Start, which switches to it, for another), Rename,
                // and Unlink last.
                if root.running && root.paired && root.tab == 2: VerticalLayout {
                    spacing: 10px;
                    vertical-stretch: 1;
                    ScrollView {
                        vertical-stretch: 1;
                        VerticalLayout {
                            alignment: start;
                            padding-right: 12px;
                            spacing: 2px;
                            for m in root.macs: Row {
                                height: 46px;
                                clickable: false;
                                if root.editing == m.fingerprint: HorizontalLayout {
                                    padding-left: 6px;
                                    padding-right: 4px;
                                    spacing: 6px;
                                    name := Field {
                                        text: m.name;
                                        accepted(t) => { root.rename(m.fingerprint, t); }
                                        init => { self.focus(); }
                                    }
                                    VerticalLayout { alignment: center; Chip { text: "Save"; clicked => { root.rename(m.fingerprint, name.text); } } }
                                    VerticalLayout { alignment: center; Chip { text: "Cancel"; clicked => { root.editing = ""; } } }
                                }
                                if root.confirming == m.fingerprint: HorizontalLayout {
                                    padding-left: 10px;
                                    padding-right: 4px;
                                    spacing: 6px;
                                    Text {
                                        text: "Unlink " + m.name + "? To use it again you link it again.";
                                        color: Look.fg;
                                        font-size: 12px;
                                        wrap: word-wrap;
                                        vertical-alignment: center;
                                        horizontal-stretch: 1;
                                    }
                                    VerticalLayout { alignment: center; Chip { text: "Unlink"; danger: true; clicked => { root.confirming = ""; root.act("forget", m.fingerprint); } } }
                                    VerticalLayout { alignment: center; Chip { text: "Cancel"; clicked => { root.confirming = ""; } } }
                                }
                                // One line: the Mac (double-click the name to rename it), whether it
                                // can be reached, then the stream signs and Unlink.
                                if root.editing != m.fingerprint && root.confirming != m.fingerprint: HorizontalLayout {
                                    padding-left: 10px;
                                    padding-right: 4px;
                                    spacing: 8px;
                                    Rectangle {
                                        horizontal-stretch: 1;
                                        min-width: 30px;
                                        // The name, and under it where the Mac stands (a status, never
                                        // an action): Online green; Locked or Busy orange; Not online
                                        // grey; nothing while not known yet.
                                        VerticalLayout {
                                            alignment: center;
                                            spacing: 1px;
                                            Text {
                                                text: m.name;
                                                color: Look.fg;
                                                font-size: 13px;
                                                font-weight: m.active ? 600 : 400;
                                                overflow: elide;
                                            }
                                            // The dot says it in colour; the words stay plain.
                                            if m.online != "" || m.via != "": HorizontalLayout {
                                                spacing: 5px;
                                                if m.online != "": VerticalLayout {
                                                    alignment: center;
                                                    Rectangle {
                                                        width: 8px;
                                                        height: 8px;
                                                        border-radius: 4px;
                                                        background: MacState.hollow(m.online) ? transparent : MacState.ink(m.online);
                                                        border-width: MacState.hollow(m.online) ? 2px : 0px;
                                                        border-color: MacState.ink(m.online);
                                                    }
                                                }
                                                Text {
                                                    text: MacState.words(m.online);
                                                    color: Look.muted;
                                                    font-size: 11px;
                                                    overflow: elide;
                                                    horizontal-stretch: 0;
                                                }
                                                if m.via != "": Via { text: m.online != "" ? "· " + m.via : m.via; horizontal-stretch: 1; }
                                            }
                                        }
                                        TouchArea {
                                            double-clicked => { Tip.leave(); root.confirming = ""; root.editing = m.fingerprint; }
                                            changed has-hover => {
                                                if (self.has-hover) { Tip.enter("Double-click to rename", self.absolute-position.x, self.absolute-position.y, self.width, self.height); } else { Tip.leave(); }
                                            }
                                        }
                                    }
                                    // Start, pause, resume and stop as the signs a player uses.
                                    if m.active: HorizontalLayout {
                                        spacing: 4px;
                                        horizontal-stretch: 0;
                                        for a in root.actions: VerticalLayout {
                                            alignment: center;
                                            Chip { text: a.label; sign: a.id; enabled: a.tip == ""; tip: a.tip != "" ? a.tip : Tip.stream(a.id); clicked => { root.act(a.id, ""); } }
                                        }
                                    }
                                    if !m.active: VerticalLayout {
                                        alignment: center;
                                        horizontal-stretch: 0;
                                        Chip {
                                            text: "Start";
                                            sign: "start";
                                            // Only a Mac that can be reached.
                                            enabled: m.online == "available" || m.online == "locked";
                                            tip: m.online == "available" || m.online == "locked" ? "Start streaming from this Mac"
                                                : m.online == "offline" ? m.name + " is not online"
                                                : m.online == "busy" ? "Stop streaming elsewhere first"
                                                : "Checking whether " + m.name + " is online";
                                            clicked => { root.act("switch", m.fingerprint); }
                                        }
                                    }
                                    VerticalLayout {
                                        alignment: center;
                                        horizontal-stretch: 0;
                                        Chip { text: "Unlink"; tip: "Unlink this Mac"; clicked => { root.editing = ""; root.confirming = m.fingerprint; } }
                                    }
                                }
                            }
                        }
                    }
                    HorizontalLayout {
                        alignment: start;
                        Btn { text: "Link a Mac…"; clicked => { root.act("pair", ""); } }
                    }
                }

                // No Mac yet, or no client to ask.
                if !root.running || !root.paired: VerticalLayout {
                    vertical-stretch: 1;
                    alignment: center;
                    spacing: 14px;
                    Label {
                        text: root.running ? "No Mac linked yet" : "Sweeflow isn't running";
                        font-size: 15px;
                        font-weight: 600;
                        horizontal-alignment: center;
                    }
                    HorizontalLayout {
                        alignment: center;
                        if root.running: Btn { text: "Link a Mac…"; primary: true; clicked => { root.act("pair", ""); } }
                        if !root.running: Btn { text: "Start Sweeflow"; primary: true; clicked => { root.start-client(); } }
                    }
                }

                // The Shortcuts window and Check for updates at the left, Quit at the right.
                if root.running && root.paired: Rectangle {
                    vertical-stretch: 0;
                    height: 34px;
                    foot := Rectangle {
                    // With a wide font (Omarchy's is monospace) the three don't fit at their usual
                    // size: the room around each one's text gives way first.
                    property <length> pad: clamp((self.width - 16px - words.preferred-width) / 3, 10px, 32px);
                    words := Text { text: "ShortcutsCheck for updatesQuit Sweeflow"; font-size: 13px; font-weight: 500; visible: false; }
                    HorizontalLayout {
                        spacing: 8px;
                        Btn { text: "Shortcuts"; pad: foot.pad; clicked => { root.act("shortcuts", ""); } }
                        Btn { text: "Check for updates"; pad: foot.pad; clicked => { root.act("update-window", ""); } }
                        Rectangle { horizontal-stretch: 1; }
                        Btn { text: "Quit Sweeflow"; pad: foot.pad; clicked => { root.quit-all(); } }
                    }
                    }
                }
            }
        }

        TipBubble { bounds-w: root.width; bounds-h: root.height; }
    }
}

const BUS_NAME: &str = "dev.sweeflow.Client";
const PATH: &str = "/dev/sweeflow/Panel";
const IFACE: &str = "dev.sweeflow.Panel";

/// The client's `State` (tray::panel_state), as much of it as this window shows.
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
struct PanelState {
    paired: bool,
    connected: bool,
    /// Stop was chosen.
    offline: bool,
    /// The Mac can be reached; None while not known yet.
    online: Option<bool>,
    status: String,
    /// What can be done about the Mac in use (Do actions), in order.
    actions: Vec<String>,
    /// The Mac is streaming to another computer: Start can't be chosen.
    #[serde(default)]
    elsewhere: bool,
    /// The Mac in use has its screen locked.
    locked: bool,
    mac: String,
    settings: Settings,
    apps: Vec<App>,
    macs: Vec<Mac>,
    open_at_login: bool,
    square: bool,
    /// The Mac's whole screen is up in a window here.
    display_shown: bool,
    /// The keys the desktop has now (keys::Keys::json); None from a client before them.
    keys: Option<ShownKeys>,
    /// This copy's version, a newer release, the Mac's version (update::panel_json).
    updates: Updates,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
struct Updates {
    update: Option<UpdateInfo>,
    mac_note: Option<String>,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
struct UpdateInfo {
    version: String,
    page: String,
    how: String,
    words: String,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
struct ShownKeys {
    open: String,
    pause: String,
    stop: String,
    menu: String,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    enabled: bool,
    exclude: Vec<String>,
    sound: bool,
    dim: bool,
    clipboard: bool,
    drag_drop: bool,
    launcher: bool,
    quality: String,
    lid: bool,
    has_lid: bool,
    lid_asking: bool,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
struct App {
    id: String,
    name: String,
    /// A file:// URL, or empty.
    icon: String,
    /// Running on the Mac: 0 not, 1-3 with or without windows.
    state: u8,
    pinned: bool,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
struct Mac {
    name: String,
    fingerprint: String,
    active: bool,
    /// Reachable on the network; None while not known yet.
    online: Option<bool>,
    /// Streaming to another computer (as it says on the network).
    busy: bool,
    /// Its screen is locked (the same).
    locked: bool,
    /// How it's reached (net::route_kind), and at what address.
    via: String,
    address: String,
}

/// How a Mac is reached, in a few words: "Remote: 100.64.0.3".
fn via_line(m: &Mac) -> String {
    crate::net::route_words(&m.via, &m.address)
}


fn parse_state(json: &str) -> Option<PanelState> {
    serde_json::from_str(json).inspect_err(|e| log::warn!("Sweeflow's state: {e}")).ok()
}

/// The heading an app is listed under.
/// Its heading. Not streaming, the Mac doesn't say which apps run: one list ("Apps") rather
/// than every app under "Not running".
fn group(app: &App, streaming: bool) -> &'static str {
    if app.pinned {
        "Pinned"
    } else if !streaming {
        "Apps"
    } else if app.state > 0 {
        "Running"
    } else {
        "Not running"
    }
}

/// The apps whose name has what's typed in it (case aside), in the order given.
fn filter_apps<'a>(apps: &'a [App], typed: &str) -> Vec<&'a App> {
    let typed = typed.trim().to_lowercase();
    apps.iter().filter(|a| a.name.to_lowercase().contains(&typed)).collect()
}

/// What the window asks of the client, in order, off the window's thread.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Set(serde_json::Value),
    Do(String, String),
    Rename(String, String),
}

fn act(action: &str, arg: &str) -> Call {
    Call::Do(action.into(), arg.into())
}

/// The call that flips the setting `key` (as `toggles` names them).
fn flip_call(key: &str, s: &PanelState) -> Option<Call> {
    let on_off = |on: bool| if on { "off" } else { "on" };
    Some(match key {
        "sound" => Call::Set(serde_json::json!({ "sound": !s.settings.sound })),
        "clipboard" => Call::Set(serde_json::json!({ "clipboard": !s.settings.clipboard })),
        "dragDrop" => Call::Set(serde_json::json!({ "dragDrop": !s.settings.drag_drop })),
        "launcher" => Call::Set(serde_json::json!({ "launcher": !s.settings.launcher })),
        // Once its password prompt is up, the prompt is the way (the Mac asks once at a time).
        "lid" if s.settings.lid_asking => return None,
        "lid" => Call::Set(serde_json::json!({ "lid": !s.settings.lid })),
        "sharp" => Call::Set(serde_json::json!({ "quality": if s.settings.quality == "light" { "sharp" } else { "light" } })),
        "square" => act("square", on_off(s.square)),
        "login" => act("login", on_off(s.open_at_login)),
        _ => return None,
    })
}

/// Stream with lid closed, on hover (here and in the Plasma widget).
const LID_TIP: &str = "Keep the MacBook available while its lid is closed. Asks for the Mac's admin password to turn it on or off.";

/// Under Stream with lid closed while the Mac's password prompt is up (here and in the widget).
const LID_WAITING: &str = "Waiting for the Mac's password";

/// The settings, in the widget's words and order.
fn toggles(s: &PanelState) -> Vec<Toggle> {
    let t = |key: &str, label: &str, on: bool, tip: &str| Toggle { key: key.into(), label: label.into(), on, tip: tip.into(), note: Default::default() };
    // The features first, named and ordered as on the setup window's last page; the tips as the
    // Plasma widget's hints.
    let mut all = vec![
        t("launcher", "Apps in launcher", s.settings.launcher, "Mac apps in the app launcher, alongside your Linux apps"),
        t("clipboard", "Shared clipboard", s.settings.clipboard, "Copy on one, paste on the other"),
        t("dragDrop", "Drag and drop files", s.settings.drag_drop, "Files, between Mac windows and Linux"),
        t("sound", "Sound passthrough", s.settings.sound, "The Mac's sound plays on this computer"),
        t("sharp", "High quality", s.settings.quality != "light", "Retina detail and 10-bit colour. Off: half the detail and 8-bit"),
        t("square", "Square window corners", s.square, "Mac windows end in square corners instead of the Mac's rounded ones"),
        t("login", "Open at login", s.open_at_login, "Start Sweeflow when you log in"),
    ];
    // A MacBook, connected: its answer (the password asked for on the Mac) comes from there.
    if s.connected && s.settings.has_lid {
        let mut lid = t("lid", "Stream with lid closed", s.settings.lid, LID_TIP);
        // As it is until the password is given (or the prompt cancelled).
        if s.settings.lid_asking {
            lid.note = LID_WAITING.into();
        }
        all.insert(6, lid);
    }
    all
}

/// Sends the calls to the client, one after another, on a thread of their own (some take a
/// moment there, as switching open at login does).
fn caller() -> mpsc::Sender<Call> {
    let (tx, rx) = mpsc::channel::<Call>();
    std::thread::spawn(move || {
        let bus = match zbus::blocking::Connection::session() {
            Ok(b) => b,
            Err(e) => return log::warn!("no D-Bus session: {e}"),
        };
        for call in rx {
            let sent = match &call {
                Call::Set(changes) => bus.call_method(Some(BUS_NAME), PATH, Some(IFACE), "Set", &(changes.to_string(),)),
                Call::Do(action, arg) => bus.call_method(Some(BUS_NAME), PATH, Some(IFACE), "Do", &(action.as_str(), arg.as_str())),
                Call::Rename(fp, name) => bus.call_method(Some(BUS_NAME), PATH, Some(IFACE), "Rename", &(fp.as_str(), name.as_str())),
            };
            if let Err(e) = sent {
                log::warn!("{call:?}: {e}");
            }
            // Quitting Sweeflow: nothing left here to show.
            if call == act("quit", "") {
                let _ = slint::quit_event_loop();
            }
        }
    });
    tx
}

/// The client's state as it is now; None when it isn't running.
fn fetch(bus: &zbus::blocking::Connection) -> Option<PanelState> {
    let reply = bus
        .call_method(Some(BUS_NAME), PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(IFACE, "State"))
        .ok()?;
    let value: zvariant::OwnedValue = reply.body().deserialize().ok()?;
    parse_state(&String::try_from(value).ok()?)
}

fn has_owner(bus: &zbus::blocking::Connection) -> bool {
    bus.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &(BUS_NAME,))
        .ok()
        .and_then(|r| r.body().deserialize::<bool>().ok())
        .unwrap_or(false)
}

/// Keeps the window up to date while it's open: every change the client signals (the value
/// comes with the signal), and the client starting or going away (looked at every second; the
/// bus answers that, not the client).
fn watch(weak: slint::Weak<PanelWin>) {
    let show = |weak: &slint::Weak<PanelWin>, state: Option<PanelState>| weak.upgrade_in_event_loop(move |ui| take(&ui, state)).is_ok();
    let bus = match zbus::blocking::Connection::session() {
        Ok(b) => b,
        Err(e) => {
            log::warn!("no D-Bus session: {e}");
            show(&weak, None);
            return;
        }
    };
    {
        let (bus, weak) = (bus.clone(), weak.clone());
        std::thread::spawn(move || {
            let changes = || -> zbus::Result<zbus::blocking::MessageIterator> {
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .sender(BUS_NAME)?
                    .path(PATH)?
                    .interface("org.freedesktop.DBus.Properties")?
                    .member("PropertiesChanged")?
                    .build();
                zbus::blocking::MessageIterator::for_match_rule(rule, &bus, None)
            };
            let changes = match changes() {
                Ok(c) => c,
                Err(e) => return log::warn!("Sweeflow's changes: {e}"),
            };
            for msg in changes.flatten() {
                type Changed = (String, HashMap<String, zvariant::OwnedValue>, Vec<String>);
                let Ok((iface, mut changed, _)) = msg.body().deserialize::<Changed>() else { continue };
                let Some(json) = changed.remove("State").filter(|_| iface == IFACE).and_then(|v| String::try_from(v).ok()) else { continue };
                if !show(&weak, parse_state(&json)) {
                    return;
                }
            }
        });
    }
    std::thread::spawn(move || {
        let mut shown_running = None;
        loop {
            let running = has_owner(&bus);
            if shown_running != Some(running) {
                let state = if running { fetch(&bus) } else { None };
                // Just started, and not answering yet: asked again in a second.
                if !running || state.is_some() {
                    shown_running = Some(running);
                }
                if !show(&weak, state) {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

/// What the window shows (UI thread only).
#[derive(Default)]
struct View {
    state: Option<PanelState>,
    typed: String,
    apps: Rc<VecModel<AppRow>>,
    macs: Rc<VecModel<MacItem>>,
    toggles: Rc<VecModel<Toggle>>,
    actions: Rc<VecModel<Action>>,
    /// The rows as last set, so a change elsewhere doesn't rebuild them (and lose a name being
    /// typed in a rename).
    app_rows: Vec<AppRow>,
    mac_rows: Vec<MacItem>,
    action_rows: Vec<Action>,
}

thread_local! {
    static VIEW: RefCell<View> = RefCell::new(View::default());
    /// App icons by file, loaded once.
    static ICONS: RefCell<HashMap<String, Option<slint::Image>>> = RefCell::new(HashMap::new());
}

fn icon(url: &str) -> Option<slint::Image> {
    let path = url.strip_prefix("file://")?;
    ICONS.with_borrow_mut(|icons| icons.entry(path.to_string()).or_insert_with(|| slint::Image::load_from_path(std::path::Path::new(path)).ok()).clone())
}

/// Why Start can't be pressed now, if it can't: the Mac is streaming to another computer, or it
/// can't be reached (or isn't known to be reachable yet).
fn start_blocked(s: &PanelState, action: &str) -> Option<String> {
    if action != "start" {
        return None;
    }
    let mac = if s.mac.is_empty() { "The Mac" } else { s.mac.as_str() };
    let busy = s.macs.iter().any(|m| m.active && m.busy);
    match (s.elsewhere, s.online) {
        (true, _) => Some(crate::tray::start_hint()),
        (false, Some(true)) if busy => Some(crate::tray::start_hint()),
        (false, Some(false)) => Some(format!("{mac} is not online")),
        (false, None) => Some(format!("Checking whether {mac} is online")),
        _ => None,
    }
}

/// Where a linked Mac stands, for the line under its name: "streaming" or "paused" (to this
/// computer), "locked", "busy" (streaming to another computer), "available", "offline", or ""
/// while not known yet. Streaming, paused and locked are known only of the Mac in use.
fn mac_state(s: &PanelState, m: &Mac) -> &'static str {
    if m.active && s.connected {
        return if s.locked { "locked" } else if s.settings.enabled { "streaming" } else { "paused" };
    }
    match m.online {
        Some(true) if m.busy || (m.active && s.elsewhere) => "busy",
        Some(true) if m.locked => "locked",
        Some(true) => "available",
        Some(false) => "offline",
        None => "",
    }
}

/// A new state from the client (None: it isn't running): shown.
fn take(ui: &PanelWin, state: Option<PanelState>) {
    VIEW.with_borrow_mut(|v| {
        v.state = state;
        show(ui, v);
    });
}

fn show(ui: &PanelWin, v: &mut View) {
    let Some(s) = v.state.clone() else {
        ui.set_running(false);
        return;
    };
    ui.set_running(true);
    ui.set_shortcut_settings(crate::tray::shortcut_settings_place().is_some());
    if let Some(k) = &s.keys {
        ui.set_key_open(crate::keys::caps_model(&k.open));
        ui.set_key_pause(crate::keys::caps_model(&k.pause));
        ui.set_key_stop(crate::keys::caps_model(&k.stop));
        if !k.menu.is_empty() {
            ui.set_key_menu(crate::keys::caps_model(&k.menu));
        }
    }
    ui.set_paired(s.paired);
    ui.set_mac_name(if s.mac.is_empty() { "Mac".into() } else { s.mac.as_str().into() });
    ui.set_status(s.status.as_str().into());
    let active_state = s.macs.iter().find(|m| m.active).map_or("", |m| mac_state(&s, m));
    ui.set_status_state(active_state.into());
    let active = s.macs.iter().find(|m| m.active);
    ui.set_status_via(active.map(via_line).unwrap_or_default().into());
    let streaming = s.connected && s.settings.enabled;
    let actions: Vec<Action> =
        s.actions
            .iter()
            .map(|a| Action {
                id: a.as_str().into(),
                label: crate::tray::action_label(a).into(),
                tip: start_blocked(&s, a).unwrap_or_default().into(),
            })
            .collect();
    // Set only when they change: a button rebuilt under the pointer loses the click.
    if actions != v.action_rows {
        v.actions.set_vec(actions.clone());
        v.action_rows = actions;
    }
    ui.set_dim_enabled(streaming && !s.settings.dim);
    ui.set_dimmed(s.settings.dim);
    ui.set_display_shown(s.display_shown);
    let u = s.updates.update.clone().unwrap_or_default();
    ui.set_update_version(u.version.as_str().into());
    ui.set_update_how(u.how.as_str().into());
    ui.set_update_words(u.words.as_str().into());
    ui.set_update_page(u.page.as_str().into());
    ui.set_mac_note(s.updates.mac_note.clone().unwrap_or_default().as_str().into());
    v.toggles.set_vec(toggles(&s));
    ui.set_app_count(s.apps.len() as i32);
    show_apps(v);
    let macs: Vec<MacItem> = s
        .macs
        .iter()
        .map(|m| MacItem {
            name: m.name.as_str().into(),
            fingerprint: m.fingerprint.as_str().into(),
            active: m.active,
            online: mac_state(&s, m).into(),
            via: via_line(m).into(),
        })
        .collect();
    if macs != v.mac_rows {
        v.macs.set_vec(macs.clone());
        v.mac_rows = macs;
        // A Mac renamed or forgotten: its row is back to normal.
        ui.set_editing(SharedString::new());
        ui.set_confirming(SharedString::new());
    }
}

/// The app rows for what's typed, each group's first under its heading.
fn show_apps(v: &mut View) {
    let Some(s) = &v.state else { return };
    let streaming = s.connected && s.settings.enabled;
    let mut last = "";
    let rows: Vec<AppRow> = filter_apps(&s.apps, &v.typed)
        .into_iter()
        .map(|a| {
            let g = group(a, streaming);
            let heading = if g != last { g } else { "" };
            last = g;
            let image = icon(&a.icon);
            AppRow {
                id: a.id.as_str().into(),
                name: a.name.as_str().into(),
                has_icon: image.is_some(),
                icon: image.unwrap_or_default(),
                initial: a.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                running: a.state > 0,
                pinned: a.pinned,
                shown: !s.settings.exclude.contains(&a.id),
                heading: heading.into(),
            }
        })
        .collect();
    if rows != v.app_rows {
        v.apps.set_vec(rows.clone());
        v.app_rows = rows;
    }
}

pub fn run() -> Result<()> {
    // One at a time, and opening it again closes it: the tray icon clicked again, or the key
    // (Cmd+Opt+O) pressed again, puts it away. The open one leaves its process id in the lock
    // file for that.
    let lock_path = crate::config::runtime_dir().join("sweeflow-panel.lock");
    let Some(_only) = crate::lock("sweeflow-panel.lock")? else {
        if let Some(pid) = std::fs::read_to_string(&lock_path).ok().and_then(|p| p.trim().parse::<u32>().ok()) {
            log::info!("Sweeflow's window is open: closing it");
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
        return Ok(());
    };
    let _ = std::fs::write(&lock_path, std::process::id().to_string());
    let ui = PanelWin::new()?;
    crate::look::apply!(ui);
    ui.set_hyprland(crate::hyprland::running());
    ui.set_omarchy(crate::hyprland::running() && crate::hyprland::omarchy());
    // Hyprland maximises any window it's told to; this one has one size.
    crate::hyprland::keep_window_size();
    // Matches sweeflow.desktop (written by `sweeflow install`): Sweeflow's logo in the title bar
    // and the taskbar. After the window is made, and before it's shown.
    crate::look::set_app_id();
    VIEW.with_borrow(|v| {
        ui.set_apps(ModelRc::from(v.apps.clone()));
        ui.set_macs(ModelRc::from(v.macs.clone()));
        ui.set_toggles(ModelRc::from(v.toggles.clone()));
        ui.set_actions(ModelRc::from(v.actions.clone()));
    });
    let calls = caller();
    let send = move |call: Call| {
        let _ = calls.send(call);
    };

    let s = send.clone();
    ui.on_act(move |action, arg| s(act(&action, &arg)));
    let s = send.clone();
    let weak = ui.as_weak();
    ui.on_flip(move |key| {
        let Some(call) = VIEW.with_borrow(|v| v.state.as_ref().and_then(|st| flip_call(&key, st))) else { return };
        // Shown flipped straight away; the client's next state confirms it.
        if let Some(ui) = weak.upgrade() {
            VIEW.with_borrow_mut(|v| {
                let mut t = toggles(v.state.as_ref().unwrap_or(&PanelState::default()));
                t.iter_mut().filter(|t| t.key == key).for_each(|t| t.on = !t.on);
                v.toggles.set_vec(t);
            });
            ui.window().request_redraw();
        }
        s(call);
    });
    let s = send.clone();
    ui.on_dim(move || s(Call::Set(serde_json::json!({ "dim": true }))));
    let s = send.clone();
    ui.on_pin(move |id, pin| s(act(if pin { "pin" } else { "unpin" }, &id)));
    let s = send.clone();
    ui.on_show_app(move |id, show| {
        let Some(mut exclude) = VIEW.with_borrow(|v| v.state.as_ref().map(|st| st.settings.exclude.clone())) else { return };
        exclude.retain(|b| *b != id.as_str());
        if !show {
            exclude.push(id.to_string());
        }
        s(Call::Set(serde_json::json!({ "exclude": exclude })));
    });
    let s = send.clone();
    let weak = ui.as_weak();
    ui.on_rename(move |fp, name| {
        if !name.trim().is_empty() {
            s(Call::Rename(fp.to_string(), name.trim().to_string()));
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_editing(SharedString::new());
        }
    });
    let weak = ui.as_weak();
    ui.on_search(move |typed| {
        VIEW.with_borrow_mut(|v| {
            v.typed = typed.to_string();
            show_apps(v);
        });
        if let Some(ui) = weak.upgrade() {
            ui.window().request_redraw();
        }
    });
    ui.on_start_client(|| {
        if let Err(e) = std::process::Command::new("systemctl").args(["--user", "start", "sweeflow"]).spawn() {
            log::warn!("starting Sweeflow: {e}");
        }
    });
    let s = send.clone();
    ui.on_quit_all(move || s(act("quit", "")));
    ui.on_dismiss(|| {
        let _ = slint::quit_event_loop();
    });

    // Clicking elsewhere closes it, as a tray popup would: once it has had the focus (a desktop
    // that opens it behind others, refusing it the focus, doesn't close it at once).
    {
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        let mut focused = false;
        ui.window().on_winit_window_event(move |_, event| {
            if let WindowEvent::Focused(now) = event {
                if *now {
                    focused = true;
                } else if focused && !crate::hyprland::running() {
                    // (Not on Hyprland: the focus follows the pointer there, so merely moving
                    // the mouse over another window closed it. It stays until Esc, or until it
                    // is opened again, which closes it.)
                    let _ = slint::quit_event_loop();
                }
            }
            EventResult::Propagate
        });
    }

    watch(ui.as_weak());
    // On X11 (Cinnamon and the like) it opens by the panel's corner, as a tray popup does,
    // not wherever the window manager puts new windows.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        // (Not shown yet: its size may still be unknown; it's 420 × 600.)
        let size = ui.window().size();
        let size = if size.width == 0 { (420, 600) } else { (size.width, size.height) };
        if let Some((x, y)) = x11_corner(size) {
            ui.window().set_position(slint::PhysicalPosition::new(x, y));
        }
        // Cinnamon may refuse it the focus even when asked, so a click elsewhere is also seen
        // as another window becoming the active one.
        close_when_another_is_active();
        // Cinnamon opens it (started by a shortcut or the tray) without the focus, and then a
        // click elsewhere couldn't close it: it asks for the focus once it's up.
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
            use slint::winit_030::WinitWindowAccessor;
            if let Some(ui) = weak.upgrade() {
                ui.window().with_winit_window(|w| w.focus_window());
            }
        });
    }
    crate::look::fit_to_screen_soon(&ui, |ui, h| ui.set_room(h));
    ui.set_swapped(crate::config::swap_cmd_opt());
    ui.run()?;
    Ok(())
}

/// Where a window this big goes on X11: in the usable area's corner by the panel (bottom right
/// under a bottom panel, top right under a top one), 8 px in, from the window manager's
/// _NET_WORKAREA.
/// X11: closes the window on a click outside it (and outside the desktop's panels, where the
/// tray icon that toggles it is), or once the window manager makes another window active. Cinnamon
/// may never give it the focus, so losing the focus alone can't be relied on.
fn close_when_another_is_active() {
    use x11rb::{
        connection::Connection,
        protocol::{
            xinput::{self, ConnectionExt as _},
            xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask},
            Event,
        },
    };
    std::thread::spawn(|| {
        let Ok((conn, screen)) = x11rb::connect(None) else { return };
        let root = conn.setup().roots[screen].root;
        let atom = |n: &[u8]| conn.intern_atom(false, n).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
        let (Some(active_atom), Some(pid_atom), Some(clients_atom), Some(workarea_atom)) =
            (atom(b"_NET_ACTIVE_WINDOW"), atom(b"_NET_WM_PID"), atom(b"_NET_CLIENT_LIST"), atom(b"_NET_WORKAREA"))
        else {
            return;
        };
        let words = |w: u32, a: u32, ty: AtomEnum, n: u32| -> Vec<u32> {
            conn.get_property(false, w, a, ty, 0, n).ok().and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default()
        };
        let active = || words(root, active_atom, AtomEnum::WINDOW, 1).first().copied().unwrap_or(0);
        let ours = |w: u32| words(w, pid_atom, AtomEnum::CARDINAL, 1).first() == Some(&std::process::id());
        let close = || {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        };
        if conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE)).is_err() {
            return;
        }
        let raw = conn.xinput_xi_query_version(2, 1).ok().and_then(|c| c.reply().ok()).is_some()
            && conn
                .xinput_xi_select_events(root, &[xinput::EventMask { deviceid: 1, mask: vec![xinput::XIEventMask::RAW_BUTTON_PRESS] }])
                .is_ok();
        let _ = conn.flush();
        // What was active as it opened (it may never become active itself).
        std::thread::sleep(std::time::Duration::from_millis(400));
        let mut opened_over = active();
        let window = words(root, clients_atom, AtomEnum::WINDOW, 1024).into_iter().rev().find(|w| ours(*w));
        // The focus, asked as a taskbar asks it (source 2): Cinnamon turns down an app's own ask
        // (winit's, above) for a window opened from the tray, and what was typed went on to the
        // window that had the keyboard before, a Mac app's.
        if let Some(w) = window.filter(|w| active() != *w) {
            use x11rb::protocol::xproto::ClientMessageEvent;
            let ev = ClientMessageEvent::new(32, w, active_atom, [2, 0, 0, 0, 0]);
            let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
            let _ = conn.flush();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let inside = |x: i32, y: i32, w: u32| -> bool {
            let Some(g) = conn.get_geometry(w).ok().and_then(|c| c.reply().ok()) else { return true };
            let Some(t) = conn.translate_coordinates(w, root, 0, 0).ok().and_then(|c| c.reply().ok()) else { return true };
            let (x0, y0) = (t.dst_x as i32, t.dst_y as i32);
            x >= x0 && y >= y0 && x < x0 + g.width as i32 && y < y0 + g.height as i32
        };
        let in_workarea = |x: i32, y: i32| -> bool {
            let a = words(root, workarea_atom, AtomEnum::CARDINAL, 4);
            let [ax, ay, aw, ah] = a[..] else { return true };
            x >= ax as i32 && y >= ay as i32 && x < (ax + aw) as i32 && y < (ay + ah) as i32
        };
        log::debug!("panel: closes on clicks elsewhere (XInput raw presses: {raw}, window {window:?})");
        while let Ok(event) = conn.wait_for_event() {
            match event {
                Event::XinputRawButtonPress(e) if e.detail <= 3 => {
                    let Some(p) = conn.query_pointer(root).ok().and_then(|c| c.reply().ok()) else { continue };
                    let (x, y) = (p.root_x as i32, p.root_y as i32);
                    if window.is_some_and(|w| !inside(x, y, w)) && in_workarea(x, y) {
                        return close();
                    }
                }
                Event::PropertyNotify(ev) if ev.atom == active_atom => {
                    let now = active();
                    if ours(now) {
                        // Active itself now: any other window after this closes it.
                        opened_over = 0;
                    } else if now != 0 && now != opened_over {
                        return close();
                    }
                }
                _ => {}
            }
        }
    });
}

fn x11_corner(size: (u32, u32)) -> Option<(i32, i32)> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?;
    let atom = conn.intern_atom(false, b"_NET_WORKAREA").ok()?.reply().ok()?.atom;
    let area = conn.get_property(false, root.root, atom, AtomEnum::CARDINAL, 0, 4).ok()?.reply().ok()?;
    let v: Vec<i32> = area.value32()?.map(|n| n as i32).collect();
    let [ax, ay, aw, ah] = v[..] else { return None };
    let (w, h) = (size.0 as i32, size.1 as i32);
    let x = ax + aw - w - 8;
    // A panel along the top leaves the usable area starting lower; otherwise it's at the bottom.
    let y = if ay > 0 { ay + 8 } else { ay + ah - h - 8 };
    Some((x.max(ax), y.max(ay)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = r#"{
        "connected": true, "locked": false, "paired": true, "mac": "Studio",
        "settings": {"enabled": true, "exclude": ["com.apple.Notes"], "sound": true, "dim": false, "clipboard": false,
                     "dragDrop": true, "launcher": true, "quality": "light", "desktop": true},
        "pending": false,
        "apps": [
            {"id": "com.apple.Terminal", "name": "Terminal", "icon": "", "taskIcon": "", "state": 0, "pinned": true},
            {"id": "com.apple.Safari", "name": "Safari", "icon": "file:///nowhere/safari.png", "taskIcon": "", "state": 3, "pinned": false},
            {"id": "com.apple.Notes", "name": "Notes", "icon": "", "taskIcon": "", "state": 0, "pinned": false},
            {"id": "com.apple.mail", "name": "Mail", "icon": "", "taskIcon": "", "state": 0, "pinned": false}
        ],
        "macs": [{"name": "Studio", "fingerprint": "ab12", "active": true, "kind": "desktop", "online": true},
                 {"name": "Air", "fingerprint": "cd34", "active": false, "kind": "", "online": null}],
        "openAtLogin": true, "square": false, "placement": {}, "pinned": ["com.apple.Terminal"],
        "offline": false, "online": true, "status": "Streaming from Studio", "actions": ["pause", "stop"], "macRefused": false, "showAt": 0
    }"#;

    #[test]
    fn parses_the_clients_state() {
        let s = parse_state(STATE).unwrap();
        assert!(s.paired && s.connected && !s.offline);
        assert_eq!(s.actions, ["pause", "stop"]);
        assert!(!s.elsewhere);
        let busy = parse_state(&STATE.replace(r#""showAt": 0"#, r#""showAt": 0, "elsewhere": true, "canStart": false"#)).unwrap();
        assert!(busy.elsewhere);
        assert_eq!(s.online, Some(true));
        assert_eq!(s.status, "Streaming from Studio");
        assert_eq!(s.mac, "Studio");
        assert!(s.settings.enabled && s.settings.drag_drop && !s.settings.clipboard);
        assert_eq!(s.settings.quality, "light");
        assert_eq!(s.settings.exclude, ["com.apple.Notes"]);
        assert_eq!(s.apps.len(), 4);
        assert_eq!(s.apps[1], App { id: "com.apple.Safari".into(), name: "Safari".into(), icon: "file:///nowhere/safari.png".into(), state: 3, pinned: false });
        assert_eq!(s.macs[0], Mac { name: "Studio".into(), fingerprint: "ab12".into(), active: true, online: Some(true), busy: false, locked: false, ..Default::default() });
        assert_eq!(s.macs[1].online, None);
        // The Mac in use streaming here; another not known yet; then each other state.
        assert_eq!(s.macs.iter().map(|m| mac_state(&s, m)).collect::<Vec<_>>(), ["streaming", ""]);
        let paused = PanelState { settings: Settings { enabled: false, ..s.settings.clone() }, ..s.clone() };
        assert_eq!(mac_state(&paused, &paused.macs[0]), "paused");
        let stopped = PanelState { connected: false, ..s.clone() };
        let other = |online, busy| Mac { online, busy, ..stopped.macs[1].clone() };
        assert_eq!(mac_state(&stopped, &stopped.macs[0]), "available");
        assert_eq!(mac_state(&stopped, &other(Some(true), true)), "busy");
        assert_eq!(mac_state(&stopped, &other(Some(false), false)), "offline");
        assert!(s.open_at_login && !s.square);
    }

    #[test]
    fn online_not_known_yet_and_missing_fields() {
        let s = parse_state(r#"{"paired": true, "offline": true, "online": null}"#).unwrap();
        assert!(s.offline && s.online.is_none() && s.apps.is_empty());
        assert!(parse_state("not json").is_none());
    }

    #[test]
    fn apps_keep_the_clients_order_and_filter_by_name() {
        let s = parse_state(STATE).unwrap();
        let names = |apps: Vec<&App>| apps.iter().map(|a| a.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(filter_apps(&s.apps, "")), ["Terminal", "Safari", "Notes", "Mail"]);
        assert_eq!(names(filter_apps(&s.apps, "  A ")), ["Terminal", "Safari", "Mail"]);
        assert_eq!(names(filter_apps(&s.apps, "fa")), ["Safari"]);
        assert_eq!(names(filter_apps(&s.apps, "TERM")), ["Terminal"]);
        assert!(filter_apps(&s.apps, "xcode").is_empty());
        let groups: Vec<&str> = s.apps.iter().map(|a| group(a, true)).collect();
        assert_eq!(groups, ["Pinned", "Running", "Not running", "Not running"]);
        // Stopped: which run isn't known, so no "Not running" for them all.
        let groups: Vec<&str> = s.apps.iter().map(|a| group(a, false)).collect();
        assert_eq!(groups, ["Pinned", "Apps", "Apps", "Apps"]);
    }

    #[test]
    fn switches_flip_what_they_show() {
        let s = parse_state(STATE).unwrap();
        assert_eq!(flip_call("clipboard", &s), Some(Call::Set(serde_json::json!({ "clipboard": true }))));
        assert_eq!(flip_call("dragDrop", &s), Some(Call::Set(serde_json::json!({ "dragDrop": false }))));
        assert_eq!(flip_call("sharp", &s), Some(Call::Set(serde_json::json!({ "quality": "sharp" }))));
        assert_eq!(flip_call("square", &s), Some(act("square", "on")));
        assert_eq!(flip_call("login", &s), Some(act("login", "off")));
        assert_eq!(flip_call("nonsense", &s), None);
        let shown: Vec<(String, bool)> = toggles(&s).into_iter().map(|t| (t.label.to_string(), t.on)).collect();
        assert_eq!(shown[0], ("Apps in launcher".to_string(), true));
        assert_eq!(shown[4], ("High quality".to_string(), false));
        assert_eq!(shown.len(), 7);
    }
}
