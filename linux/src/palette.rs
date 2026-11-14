//! The Global menu (`sweeflow menu`): the menu bar of the Mac app last used here, as an overlay
//! like a launcher's (see Palette's `overlay`), for desktops that show no app menus (Omarchy,
//! Hyprland) and on Plasma and GNOME alike. It opens
//! on the menu bar's menus and goes into them as the Mac's do; typing searches all of them at
//! once. A process of its own, like the pairing wizard and the prompts, so its event loop never
//! shares a thread with the streaming client's. It asks the running client for the menu and has
//! it run the chosen item (panel.rs, menus.rs), which presses it on the Mac exactly as Plasma's
//! Global Menu does. Cmd+Opt+U in a Mac window opens it (ui.rs), and so does the tray.

use std::cell::RefCell;

use anyhow::{Context, Result};
use serde::Deserialize;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::slint! {
    import { ListView } from "std-widgets.slint";
    import { Look, Dim } from "look.slint";
    export { Look }

    // One row: an item (kind 0), a submenu (1) or a separator (2). `path` is the submenus it's
    // in, shown before its name in search results ("File › ").
    export struct MenuRow {
        kind: int,
        path: string,
        name: string,
        shortcut: string,
        enabled: bool,
        checked: bool,
        height: length,
    }

    // The search field: look.slint's Field, telling each edit as it's typed, and whether it has
    // the keyboard (it always does while the window is active: nothing else here takes it).
    component Search inherits Rectangle {
        in property <string> placeholder;
        in-out property <string> text <=> input.text;
        out property <bool> focused: input.has-focus;
        callback edited(string);
        height: 36px;
        clip: true;
        border-radius: Look.radius;
        border-width: 1px;
        border-color: input.has-focus ? Look.accent : Look.line;
        background: Look.surface;
        forward-focus: input;
        Text {
            x: 10px;
            width: parent.width - 20px;
            height: parent.height;
            visible: input.text == "";
            text: root.placeholder;
            color: Look.muted;
            font-size: 14px;
            vertical-alignment: center;
            overflow: elide;
        }
        input := TextInput {
            x: 10px;
            width: parent.width - 20px;
            height: parent.height;
            single-line: true;
            vertical-alignment: center;
            color: Look.fg;
            font-size: 14px;
            selection-background-color: Look.accent;
            selection-foreground-color: Look.on-accent;
            edited => { root.edited(self.text); }
        }
    }

    // A row as the Mac draws a menu: a tick before checked items, the name, its shortcut on
    // the right and a "›" after submenus; disabled ones dimmed, separators a thin line.
    component Entry inherits Rectangle {
        in property <MenuRow> entry;
        in property <bool> current;
        callback clicked;
        property <bool> live: root.entry.enabled && root.entry.kind != 2;
        property <color> ink: root.current ? Look.on-accent : root.entry.enabled ? Look.fg : Look.muted;
        property <color> dim: root.current ? Look.on-accent.transparentize(0.25) : Look.muted;
        border-radius: Look.radius;
        background: root.current ? Look.accent : touch.has-hover && root.live ? Look.surface : transparent;
        touch := TouchArea {
            enabled: root.live;
            mouse-cursor: root.live ? pointer : default;
            clicked => { root.clicked(); }
        }
        if root.entry.kind == 2: Rectangle {
            x: 10px;
            // Clear of the scroll bar, which is drawn over the list.
            width: parent.width - 26px;
            y: (parent.height - 1px) / 2;
            height: 1px;
            background: Look.line;
        }
        if root.entry.kind != 2: HorizontalLayout {
            padding-left: 4px;
            // Room for the scroll bar.
            padding-right: 16px;
            spacing: 0px;
            Text {
                width: 20px;
                text: root.entry.checked ? "✓" : "";
                color: root.ink;
                font-size: 13px;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
            Text {
                text: root.entry.path;
                color: root.dim;
                font-size: 13px;
                vertical-alignment: center;
                horizontal-stretch: 0;
            }
            Text {
                text: root.entry.name;
                color: root.ink;
                font-size: 13px;
                vertical-alignment: center;
                overflow: elide;
                horizontal-stretch: 1;
            }
            Text {
                text: root.entry.shortcut;
                color: root.dim;
                font-size: 12px;
                vertical-alignment: center;
                horizontal-alignment: right;
                horizontal-stretch: 0;
            }
            Text {
                width: 20px;
                text: root.entry.kind == 1 ? "›" : "";
                color: root.dim;
                font-size: 15px;
                vertical-alignment: center;
                horizontal-alignment: right;
            }
        }
    }

    export component Palette inherits Window {
        in property <string> app;
        // Where in the menu it is ("File › Open Recent"); empty on the menu bar's menus.
        in property <string> trail;
        in property <[MenuRow]> rows;
        // No menu at all (as opposed to nothing matching what's typed).
        in property <bool> empty: true;
        in-out property <int> selected;
        // Where the highlighted row is in the list (rows aren't all as high).
        in property <length> selected-top;
        in property <length> selected-height;
        in-out property <string> typed <=> field.text;
        // The window is the active one.
        out property <bool> active: field.focused;
        // An overlay: the window covers the screen (full screen), see-through and dimmed, with
        // the menu in the middle; a click beside the menu closes it. Otherwise (Hyprland, tiling
        // desktops) the window is just the menu, which the desktop floats and centres.
        in property <bool> overlay;
        callback search(string);
        // Highlight up or down, by rows (none highlighted: from the top or the bottom).
        callback step(int);
        // Enter (true) or a click (false): run the item or go into the submenu.
        callback choose(int, bool);
        // Right: into the submenu, if that row is one.
        callback open(int);
        // Left or Backspace: up a level.
        callback back();
        callback dismiss();
        callback focus-lost();

        title: root.app != "" ? root.app + " - Global Menu" : "Global Menu";
        icon: @image-url("../../assets/sweeflow-app.svg");
        background: root.overlay ? transparent : Look.bg;
        default-font-family: Look.font;
        no-frame: true;
        always-on-top: true;
        // The menu's own size. As a window of its own, a fixed one: a dialog, which tiling
        // window managers leave floating. As an overlay the window is the screen's size.
        property <length> card-width: 560px;
        property <length> card-height: 420px;
        preferred-width: root.card-width;
        preferred-height: root.card-height;
        min-width: root.overlay ? 0px : root.card-width;
        max-width: root.overlay ? 100000px : root.card-width;
        min-height: root.overlay ? 0px : root.card-height;
        max-height: root.overlay ? 100000px : root.card-height;
        forward-focus: field;

        // Only once it's been active: before its first focus it isn't, and must stay open.
        changed active => {
            if (!self.active) {
                root.focus-lost();
            }
        }

        // The highlighted row into view, scrolling as little as it takes.
        public function show-selected() {
            // Not laid out yet (before the window shows): it starts at the top anyway.
            if (list.visible-height <= 0) {
                return;
            }
            if (root.selected-top + list.content-y < 0) {
                list.content-y = -root.selected-top;
            } else if (root.selected-top + root.selected-height + list.content-y > list.visible-height) {
                list.content-y = list.visible-height - root.selected-top - root.selected-height;
            }
        }

        // Beside the menu: dims what's under it, and a click there closes it.
        if root.overlay: Rectangle {
            background: #00000040;
            TouchArea {
                clicked => { root.dismiss(); }
            }
        }

        Rectangle {
            width: root.overlay ? min(root.card-width, root.width - 32px) : root.width;
            height: root.overlay ? min(root.card-height, root.height - 32px) : root.height;
            // A little above the middle, where a launcher sits.
            x: (root.width - self.width) / 2;
            y: root.overlay ? max(16px, (root.height - self.height) * 0.4) : 0px;
            background: Look.bg;
            border-radius: root.overlay ? Look.radius + 4px : 0px;
            border-width: root.overlay ? 1px : 0px;
            border-color: Look.line;
            drop-shadow-blur: root.overlay ? 32px : 0px;
            drop-shadow-offset-y: 8px;
            drop-shadow-color: #00000066;
            // Clicks on the menu (between its rows) don't reach the backdrop.
            TouchArea {}
        VerticalLayout {
            padding: 16px;
            spacing: 12px;
            // The app inside the window too: a tiling desktop (Hyprland) draws no title bar.
            VerticalLayout {
                spacing: 2px;
                vertical-stretch: 0;
                Text {
                    text: root.title;
                    color: Look.fg;
                    font-size: 15px;
                    font-weight: 600;
                    overflow: elide;
                }
                // As high when empty, so the list doesn't move going in and out.
                Dim {
                    height: 16px;
                    text: root.trail;
                    font-size: 12px;
                    wrap: no-wrap;
                    overflow: elide;
                }
            }
            // Catches the keys that move through the menu before the field gets them. (As high
            // as the field: left to the layout, it took a third of the window.) Left and Right
            // are the field's own while something's typed.
            FocusScope {
                height: 36px;
                vertical-stretch: 0;
                capture-key-pressed(event) => {
                    if (event.text == Key.UpArrow) {
                        root.step(-1);
                        return accept;
                    }
                    if (event.text == Key.DownArrow) {
                        root.step(1);
                        return accept;
                    }
                    if (event.text == Key.PageUp) {
                        root.step(-10);
                        return accept;
                    }
                    if (event.text == Key.PageDown) {
                        root.step(10);
                        return accept;
                    }
                    if (event.text == Key.Return) {
                        if (root.selected >= 0) {
                            root.choose(root.selected, true);
                        }
                        return accept;
                    }
                    if (event.text == Key.RightArrow && field.text == "") {
                        if (root.selected >= 0) {
                            root.open(root.selected);
                        }
                        return accept;
                    }
                    if ((event.text == Key.LeftArrow || event.text == Key.Backspace) && field.text == "") {
                        root.back();
                        return accept;
                    }
                    if (event.text == Key.Escape) {
                        root.dismiss();
                        return accept;
                    }
                    return reject;
                }
                field := Search {
                    placeholder: "Search all menus";
                    edited(text) => { root.search(text); }
                }
            }
            Rectangle {
                vertical-stretch: 1;
                list := ListView {
                    width: parent.width;
                    height: parent.height;
                    visible: root.rows.length > 0;
                    for row[i] in root.rows: Entry {
                        height: row.height;
                        entry: row;
                        current: i == root.selected;
                        clicked => { root.choose(i, false); }
                    }
                }
                Dim {
                    width: parent.width;
                    height: parent.height;
                    visible: root.rows.length == 0;
                    text: root.empty ? "No Mac app menu to show" : field.text != "" ? "No menu item matches" : "Nothing in this menu";
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
            }
        }
        }
    }
}

/// The menu as the client gives it (menus::palette_menu).
#[derive(Deserialize, Default)]
struct Menu {
    app: String,
    pid: i32,
    revision: u32,
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    id: i32,
    #[serde(default)]
    label: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    separator: bool,
    #[serde(default)]
    checked: bool,
    #[serde(default)]
    submenu: bool,
    #[serde(default)]
    shortcut: String,
    #[serde(default)]
    children: Vec<Item>,
}

fn yes() -> bool {
    true
}

impl Item {
    /// A submenu, also while it has nothing in it (Open Recent, empty).
    fn is_submenu(&self) -> bool {
        self.submenu || !self.children.is_empty()
    }

    fn name(&self) -> &str {
        self.label.trim()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Item,
    Submenu,
    Separator,
}

/// A row of the list.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    kind: Kind,
    /// Where it is in the menu: its index in each submenu down to it.
    at: Vec<usize>,
    /// The submenus it's in, by name ("File", "Open Recent").
    path: Vec<String>,
    name: String,
    id: i32,
    enabled: bool,
    checked: bool,
    shortcut: String,
}

impl Row {
    /// Its path and name, as searched and logged ("File › Open Recent").
    fn text(&self) -> String {
        self.path.iter().map(String::as_str).chain([self.name.as_str()]).collect::<Vec<_>>().join(" › ")
    }

    /// The keyboard stops on it and a click does something.
    fn choosable(&self) -> bool {
        self.enabled && self.kind != Kind::Separator
    }

    fn height(&self) -> f32 {
        if self.kind == Kind::Separator { 9.0 } else { 30.0 }
    }
}

/// The item at `at`.
fn node<'a>(items: &'a [Item], at: &[usize]) -> Option<&'a Item> {
    let (&first, rest) = at.split_first()?;
    let mut it = items.get(first)?;
    for &i in rest {
        it = it.children.get(i)?;
    }
    Some(it)
}

/// The names of the submenus down to `at`, the submenu shown: where it is.
fn trail(items: &[Item], at: &[usize]) -> Vec<String> {
    (1..=at.len()).filter_map(|n| node(items, &at[..n])).map(Item::name).filter(|n| !n.is_empty()).map(String::from).collect()
}

/// What the submenu at `at` shows (the menu bar's menus when `at` is empty), as the Mac shows
/// it: in order, with separators (never two together, nor at either end) and disabled items.
fn level(items: &[Item], at: &[usize]) -> Vec<Row> {
    fn add(items: &[Item], at: &[usize], path: &[String], enabled: bool, rows: &mut Vec<Row>) {
        for (i, it) in items.iter().enumerate() {
            let here = [at, &[i]].concat();
            let enabled = enabled && it.enabled;
            let name = it.name();
            let kind = if it.separator {
                Kind::Separator
            } else if it.is_submenu() {
                if name.is_empty() {
                    // An untitled submenu: its items in its place, as if they were this menu's.
                    add(&it.children, &here, path, enabled, rows);
                    continue;
                }
                Kind::Submenu
            } else if name.is_empty() {
                continue;
            } else {
                Kind::Item
            };
            if kind == Kind::Separator && rows.last().is_none_or(|r| r.kind == Kind::Separator) {
                continue;
            }
            rows.push(Row {
                kind,
                at: here,
                path: path.to_vec(),
                name: if kind == Kind::Separator { String::new() } else { name.to_string() },
                id: it.id,
                enabled,
                checked: it.checked,
                shortcut: if kind == Kind::Item { shown_shortcut(&it.shortcut) } else { String::new() },
            });
        }
    }
    let here = if at.is_empty() { Some(items) } else { node(items, at).map(|n| n.children.as_slice()) };
    let mut rows = Vec::new();
    if let Some(here) = here {
        // A disabled submenu can't be gone into (the keyboard skips it): its items are as it says.
        add(here, at, &trail(items, at), true, &mut rows);
    }
    if rows.last().is_some_and(|r| r.kind == Kind::Separator) {
        rows.pop();
    }
    rows
}

/// Everything that can be searched for, in menu order: the items and submenus that can be
/// chosen, at every level (a submenu, then what's in it). Not separators, disabled items, or
/// anything inside a disabled submenu, which the Mac won't open.
fn everything(items: &[Item]) -> Vec<Row> {
    fn walk(items: &[Item], at: &[usize], path: &mut Vec<String>, rows: &mut Vec<Row>) {
        for (i, it) in items.iter().enumerate().filter(|(_, it)| !it.separator && it.enabled) {
            let here = [at, &[i]].concat();
            let name = it.name().to_string();
            let row = |kind, shortcut| Row { kind, at: here.clone(), path: path.clone(), name: name.clone(), id: it.id, enabled: true, checked: it.checked, shortcut };
            if it.is_submenu() {
                // An untitled submenu adds nothing to its items' paths, and isn't one to go into.
                let titled = !name.is_empty();
                if titled {
                    rows.push(row(Kind::Submenu, String::new()));
                    path.push(name.clone());
                }
                walk(&it.children, &here, path, rows);
                if titled {
                    path.pop();
                }
            } else if !name.is_empty() {
                rows.push(row(Kind::Item, shown_shortcut(&it.shortcut)));
            }
        }
    }
    let mut rows = Vec::new();
    walk(items, &[], &mut Vec::new(), &mut rows);
    rows
}

/// A shortcut as the Mac shows it: ⌃ Control, ⌥ Option (Alt), ⇧ Shift and ⌘ Command (Super),
/// in that order, then the key ("⇧⌘N"). From the keys pressed here, as menus.rs gives them
/// ("Shift+Super+N"). A Mac key equivalent can be one of AppKit's private characters (the arrow
/// and function keys), which no font here draws: better none than a box.
fn shown_shortcut(shortcut: &str) -> String {
    if shortcut.chars().any(|c| ('\u{e000}'..='\u{f8ff}').contains(&c)) {
        return String::new();
    }
    let mut key = shortcut;
    let mut held = [false; 4];
    let names = ["Control+", "Alt+", "Shift+", "Super+"];
    while let Some((i, rest)) = names.iter().enumerate().find_map(|(i, n)| key.strip_prefix(n).map(|rest| (i, rest))) {
        held[i] = true;
        key = rest;
    }
    let key = match key {
        "" => return String::new(),
        "Backspace" => "⌫",
        "Escape" => "⎋",
        "Return" => "↩",
        "Space" => "Space",
        "\t" => "⇥",
        // Other control characters have no sign to show.
        k if k.chars().any(char::is_control) => return String::new(),
        k => k,
    };
    let signs: String = ["⌃", "⌥", "⇧", "⌘"].iter().zip(held).filter(|(_, h)| *h).map(|(s, _)| *s).collect();
    signs + key
}

/// The rows to show for what's typed, as indexes into `rows`: those with every typed word
/// somewhere in their path or name (case aside), the ones whose own name starts with the text
/// first, in menu order otherwise. Nothing typed: all of them.
fn filter(rows: &[Row], typed: &str) -> Vec<usize> {
    let words: Vec<String> = typed.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return (0..rows.len()).collect();
    }
    let start = words.join(" ");
    let mut hits: Vec<(bool, usize)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let text = r.text().to_lowercase();
            words.iter().all(|w| text.contains(w.as_str())).then(|| (!r.name.to_lowercase().starts_with(&start), i))
        })
        .collect();
    // Stable, so menu order holds within each group.
    hits.sort();
    hits.into_iter().map(|(_, i)| i).collect()
}

/// What the list shows: the submenu at `at`, or, with something typed, what matches it from
/// anywhere in the menu.
fn rows_for(items: &[Item], at: &[usize], typed: &str) -> Vec<Row> {
    if typed.trim().is_empty() {
        return level(items, at);
    }
    let all = everything(items);
    filter(&all, typed).into_iter().map(|i| all[i].clone()).collect()
}

/// What choosing a row (Enter, a click) does.
#[derive(Debug, PartialEq)]
enum Choice {
    Run(Row),
    /// Go into the submenu at that place.
    Open(Vec<usize>),
    Nothing,
}

fn choice(row: Option<&Row>) -> Choice {
    match row {
        Some(r) if r.choosable() && r.kind == Kind::Submenu => Choice::Open(r.at.clone()),
        Some(r) if r.choosable() => Choice::Run(r.clone()),
        _ => Choice::Nothing,
    }
}

/// The submenu one level up from `at`: past untitled ones, whose items show as their parent's.
fn up(items: &[Item], at: &[usize]) -> Vec<usize> {
    let mut at = at.to_vec();
    at.pop();
    while !at.is_empty() && node(items, &at).is_some_and(|n| n.name().is_empty()) {
        at.pop();
    }
    at
}

/// The submenu reached by these names from the menu bar, in a menu sent afresh.
fn find_level(items: &[Item], names: &[String]) -> Option<Vec<usize>> {
    let mut at = Vec::new();
    for name in names {
        at = level(items, &at).into_iter().find(|r| r.kind == Kind::Submenu && r.enabled && &r.name == name)?.at;
    }
    Some(at)
}

/// The row the highlight lands on moving `by` rows from `from`: only rows that can be chosen,
/// stopping at either end.
fn step(rows: &[Row], from: usize, by: i32) -> usize {
    let Some(last) = rows.len().checked_sub(1) else { return 0 };
    let target = (from as i64 + i64::from(by)).clamp(0, last as i64) as usize;
    let down = || (target..rows.len()).find(|&i| rows[i].choosable());
    let back = || (0..=target).rev().find(|&i| rows[i].choosable());
    let found = if by >= 0 { down().or_else(back) } else { back().or_else(down) };
    found.unwrap_or(from.min(last))
}

/// The first row the keyboard can stop on.
fn first_choosable(rows: &[Row]) -> usize {
    rows.iter().position(Row::choosable).unwrap_or(0)
}

/// Where the arrow keys take the highlight: from the row highlighted, or with none (as it
/// opens, before a key) to the first row going down and the last going up.
fn step_from(rows: &[Row], from: Option<usize>, by: i32) -> Option<usize> {
    let found = match from {
        Some(from) => step(rows, from, by),
        None if by >= 0 => step(rows, 0, 0),
        None => step(rows, rows.len().saturating_sub(1), 0),
    };
    rows.get(found).filter(|r| r.choosable()).map(|_| found)
}

/// The row highlighted as rows are shown: `keep`, if it's still there; otherwise the first
/// that can be chosen, once the keyboard has been used (an arrow key, or typing: the best match,
/// so Enter runs it); none before that, as the menu opens (the mouse highlights what it's on).
fn highlight(rows: &[Row], keep: Option<usize>, keyboard: bool) -> Option<usize> {
    keep.or_else(|| keyboard.then(|| first_choosable(rows)).filter(|&i| rows.get(i).is_some_and(Row::choosable)))
}

/// Whether the Global menu opens as an overlay over the whole screen (see Palette's
/// `overlay`): on desktops that draw a see-through full screen window over the others (Plasma,
/// GNOME and the desktops built on GNOME's window manager). Not on Hyprland, nor on the tiling
/// desktops, which show a full screen window on its own (on black): there it's a window of its
/// own, which they float and centre.
fn overlay_here(desktop: &str, hyprland: bool) -> bool {
    let desktop = desktop.to_lowercase();
    // Not GNOME: its windows get no see-through background there (the overlay came out black),
    // so the Global Menu is a window of its own size, as on Hyprland.
    !hyprland && desktop.split(':').any(|d| ["kde", "cinnamon", "x-cinnamon", "budgie", "budgie-desktop", "unity", "pantheon"].contains(&d))
}

const BUS_NAME: &str = "dev.sweeflow.Client";
const PATH: &str = "/dev/sweeflow/Panel";
const IFACE: &str = "dev.sweeflow.Panel";

/// The menu from the running client.
fn fetch(bus: &zbus::blocking::Connection, refresh: bool) -> Result<Menu> {
    let reply = bus.call_method(Some(BUS_NAME), PATH, Some(IFACE), "Menu", &(refresh,)).context("asking Sweeflow for the menu")?;
    let json: String = reply.body().deserialize()?;
    Ok(serde_json::from_str(&json)?)
}

/// Has the client run an item; false when the menu changed since `revision`.
fn activate(bus: &zbus::blocking::Connection, pid: i32, revision: u32, id: i32) -> Result<bool> {
    let reply = bus.call_method(Some(BUS_NAME), PATH, Some(IFACE), "MenuActivate", &(pid, revision, id))?;
    Ok(reply.body().deserialize()?)
}

/// What the window shows (UI thread only).
#[derive(Default)]
struct State {
    app: String,
    pid: i32,
    revision: u32,
    items: Vec<Item>,
    /// The submenu shown; empty: the menu bar's menus.
    at: Vec<usize>,
    typed: String,
    rows: Vec<Row>,
    /// The keyboard has moved through the menu or searched it: rows shown from now on come
    /// with one highlighted.
    keyboard: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// A row kept highlighted across a change, by its path and name.
type Keep = Option<(Vec<String>, String)>;

/// A new menu (or a fresher one): shown, keeping what's typed, the submenu shown and, when
/// they're still there, the highlighted row.
fn take_menu(ui: &Palette, menu: Menu) {
    STATE.with_borrow_mut(|s| {
        let keep = usize::try_from(ui.get_selected()).ok().and_then(|i| s.rows.get(i)).map(|r| (r.path.clone(), r.name.clone()));
        let names = trail(&s.items, &s.at);
        s.items = menu.items;
        s.app = menu.app;
        s.pid = menu.pid;
        s.revision = menu.revision;
        s.at = find_level(&s.items, &names).unwrap_or_default();
        ui.set_app(s.app.as_str().into());
        ui.set_empty(level(&s.items, &[]).is_empty());
        show(ui, s, keep);
    });
}

/// Shows the rows for where it is and what's typed, highlighting `keep` if it's among them (see
/// `highlight` otherwise).
fn show(ui: &Palette, s: &mut State, keep: Keep) {
    s.rows = rows_for(&s.items, &s.at, &s.typed);
    let kept = keep.and_then(|(path, name)| s.rows.iter().position(|r| r.choosable() && r.path == path && r.name == name));
    let selected = highlight(&s.rows, kept, s.keyboard);
    let searching = !s.typed.trim().is_empty();
    let rows: Vec<MenuRow> = s
        .rows
        .iter()
        .map(|r| MenuRow {
            kind: match r.kind {
                Kind::Item => 0,
                Kind::Submenu => 1,
                Kind::Separator => 2,
            },
            // Inside a menu its items are by name alone; search results come from anywhere.
            path: if searching && !r.path.is_empty() { format!("{} › ", r.path.join(" › ")).into() } else { Default::default() },
            name: r.name.as_str().into(),
            shortcut: r.shortcut.as_str().into(),
            enabled: r.enabled,
            checked: r.checked,
            height: r.height(),
        })
        .collect();
    ui.set_trail(trail(&s.items, &s.at).join(" › ").into());
    ui.set_rows(ModelRc::new(VecModel::from(rows)));
    select(ui, s, selected);
}

/// Highlights row `index` and scrolls it into view; None: no row.
fn select(ui: &Palette, s: &State, index: Option<usize>) {
    let Some(index) = index else {
        ui.set_selected(-1);
        return;
    };
    ui.set_selected(index as i32);
    ui.set_selected_top(s.rows.iter().take(index).map(Row::height).sum());
    ui.set_selected_height(s.rows.get(index).map_or(0.0, Row::height));
    ui.invoke_show_selected();
}

/// Goes into the submenu at `at`, from the menu or from a search (which is then cleared).
fn open(ui: &Palette, s: &mut State, at: Vec<usize>) {
    s.at = at;
    s.typed.clear();
    ui.set_typed("".into());
    show(ui, s, None);
}

/// Runs an item. Should the menu have changed since it was shown (the Mac sends it afresh after
/// the refresh asked for on opening), the same item is looked for in the new one.
fn run_row(bus: &zbus::blocking::Connection, pid: i32, revision: u32, row: &Row) {
    let ran = match activate(bus, pid, revision, row.id) {
        Ok(true) => Ok(true),
        Ok(false) => fetch(bus, false).and_then(|menu| {
            match everything(&menu.items).into_iter().find(|r| r.kind == Kind::Item && r.path == row.path && r.name == row.name) {
                Some(again) if menu.pid == pid => activate(bus, pid, menu.revision, again.id),
                _ => Ok(false),
            }
        }),
        Err(e) => Err(e),
    };
    match ran {
        Ok(true) => log::info!("menu: {}", row.text()),
        Ok(false) => log::warn!("menu: {} is no longer there", row.text()),
        Err(e) => log::warn!("menu: {}: {e:#}", row.text()),
    }
}

pub fn run() -> Result<()> {
    // One at a time: the key pressed again while it's open does nothing.
    let Some(_only) = crate::lock("sweeflow-menu.lock")? else {
        log::info!("the Global Menu is already open");
        return Ok(());
    };
    let bus = zbus::blocking::Connection::session().inspect_err(|e| log::warn!("no D-Bus session: {e}")).ok();
    // Asked for before the window shows, so it doesn't open empty. The refresh brings the Mac's
    // current state of the items a moment later (it last sent them when the window got focus).
    let menu = bus.as_ref().map_or_else(|| Ok(Menu::default()), |b| fetch(b, true)).unwrap_or_else(|e| {
        log::warn!("{e:#}");
        Menu::default()
    });
    let ui = Palette::new()?;
    crate::look::apply!(ui);
    let overlay = overlay_here(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(), crate::hyprland::running());
    ui.set_overlay(overlay);
    if overlay {
        // On the screen the desktop opens new windows on: the one with the pointer or the
        // focused window, on Plasma and GNOME.
        ui.window().set_fullscreen(true);
    } else {
        crate::hyprland::keep_window_size();
    }
    // Matches sweeflow.desktop (written by `sweeflow install`): Sweeflow's logo in the title bar
    // and the taskbar. After the window is made, and before it's shown.
    if let Err(e) = slint::set_xdg_app_id("sweeflow") {
        log::warn!("app id: {e}");
    }
    let (pid, mut revision) = (menu.pid, menu.revision);
    take_menu(&ui, menu);

    // The fresher menu, when it comes (or the first, if the client didn't have it yet).
    if let Some(bus) = bus.clone().filter(|_| pid != 0) {
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(150));
                let Ok(menu) = fetch(&bus, false) else { return };
                if menu.pid != pid || menu.revision == revision {
                    continue;
                }
                revision = menu.revision;
                if weak.upgrade_in_event_loop(move |ui| take_menu(&ui, menu)).is_err() {
                    return;
                }
            }
        });
    }

    let weak = ui.as_weak();
    ui.on_search(move |typed| {
        let Some(ui) = weak.upgrade() else { return };
        STATE.with_borrow_mut(|s| {
            s.typed = typed.to_string();
            s.keyboard = true;
            show(&ui, s, None);
        });
    });
    let weak = ui.as_weak();
    ui.on_step(move |by| {
        let Some(ui) = weak.upgrade() else { return };
        STATE.with_borrow_mut(|s| {
            s.keyboard = true;
            select(&ui, s, step_from(&s.rows, usize::try_from(ui.get_selected()).ok(), by));
        });
    });
    let weak = ui.as_weak();
    ui.on_choose(move |index, by_key| {
        let Some(ui) = weak.upgrade() else { return };
        // Into a submenu by a click: nothing highlighted there either, until a key.
        let picked = STATE.with_borrow_mut(|s| {
            s.keyboard = by_key;
            choice(s.rows.get(index as usize))
        });
        match picked {
            Choice::Run(row) => {
                let (pid, revision) = STATE.with_borrow(|s| (s.pid, s.revision));
                if let Some(bus) = &bus {
                    run_row(bus, pid, revision, &row);
                }
                let _ = slint::quit_event_loop();
            }
            Choice::Open(at) => STATE.with_borrow_mut(|s| open(&ui, s, at)),
            Choice::Nothing => {}
        }
    });
    let weak = ui.as_weak();
    ui.on_open(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        STATE.with_borrow_mut(|s| {
            s.keyboard = true;
            if let Choice::Open(at) = choice(s.rows.get(index as usize)) {
                open(&ui, s, at);
            }
        });
    });
    let weak = ui.as_weak();
    ui.on_back(move || {
        let Some(ui) = weak.upgrade() else { return };
        STATE.with_borrow_mut(|s| {
            if s.at.is_empty() {
                return;
            }
            s.keyboard = true;
            // Back on the submenu it came out of.
            let mut path = trail(&s.items, &s.at);
            let keep = path.pop().map(|name| (path, name));
            s.at = up(&s.items, &s.at);
            show(&ui, s, keep);
        });
    });
    ui.on_dismiss(|| {
        let _ = slint::quit_event_loop();
    });
    // Closes when another window takes the focus (a click elsewhere, a launcher, switching
    // windows): it's a menu. Looked at again a moment later, so a passing flicker of focus
    // doesn't close it.
    let weak = ui.as_weak();
    ui.on_focus_lost(move || {
        let weak = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
            if weak.upgrade().is_some_and(|ui| !ui.get_active()) {
                log::debug!("the Global Menu lost the focus: closing");
                let _ = slint::quit_event_loop();
            }
        });
    });
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i32, label: &str) -> Item {
        Item {
            id,
            label: label.into(),
            enabled: true,
            separator: false,
            checked: false,
            submenu: false,
            shortcut: String::new(),
            children: Vec::new(),
        }
    }

    fn sub(id: i32, label: &str, children: Vec<Item>) -> Item {
        Item { submenu: true, children, ..item(id, label) }
    }

    fn sep(id: i32) -> Item {
        Item { separator: true, ..item(id, "") }
    }

    fn menu() -> Vec<Item> {
        vec![
            sub(1, "Safari", vec![item(2, "About Safari"), sep(3), item(4, "Settings…")]),
            sub(
                5,
                "File",
                vec![
                    Item { shortcut: "Super+N".into(), ..item(6, "New Window") },
                    Item { shortcut: "Super+T".into(), ..item(7, "New Tab") },
                    sub(8, "Open Recent", vec![]),
                    sep(17),
                    Item { enabled: false, ..item(9, "Close Tab") },
                ],
            ),
            sub(
                10,
                "View",
                vec![
                    Item { checked: true, ..item(11, "Show Tab Overview") },
                    Item { enabled: false, ..sub(12, "Text Encoding", vec![item(13, "Unicode")]) },
                    sub(14, "Zoom", vec![item(15, "Zoom In"), item(16, "Actual Size")]),
                ],
            ),
        ]
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| if r.kind == Kind::Separator { "---".into() } else { r.name.clone() }).collect()
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn the_top_level_is_the_menu_bar() {
        let rows = level(&menu(), &[]);
        assert_eq!(names(&rows), ["Safari", "File", "View"]);
        assert!(rows.iter().all(|r| r.kind == Kind::Submenu && r.path.is_empty() && r.choosable()));
        assert_eq!(rows[1].at, [1]);
    }

    #[test]
    fn a_menu_shows_as_the_mac_does() {
        let items = menu();
        let file = level(&items, &[1]);
        assert_eq!(names(&file), ["New Window", "New Tab", "Open Recent", "---", "Close Tab"]);
        assert_eq!(file[0].shortcut, "⌘N");
        assert_eq!(file[0].path, ["File"]);
        assert_eq!(file[2].kind, Kind::Submenu);
        assert!(!file[3].choosable());
        assert!(!file[4].enabled && !file[4].choosable());
        let view = level(&items, &[2]);
        assert!(view[0].checked);
        // Disabled submenus show, dimmed, and can't be gone into.
        assert_eq!(view[1].kind, Kind::Submenu);
        assert!(!view[1].choosable());
        assert_eq!(names(&level(&items, &[2, 2])), ["Zoom In", "Actual Size"]);
        assert_eq!(level(&items, &[2, 2])[0].path, ["View", "Zoom"]);
        assert!(level(&items, &[1, 2]).is_empty());
        assert!(level(&items, &[7]).is_empty());
    }

    #[test]
    fn separators_never_double_or_end_a_menu() {
        let items = vec![sub(1, "Edit", vec![sep(2), item(3, "Undo"), sep(4), sep(5), item(6, "Cut"), sep(7)])];
        assert_eq!(names(&level(&items, &[0])), ["Undo", "---", "Cut"]);
    }

    #[test]
    fn untitled_submenus_show_their_items_in_their_place() {
        let items = vec![sub(1, "Edit", vec![item(2, "Undo"), sub(3, "", vec![item(4, "Inside"), item(5, "  ")]), item(6, "Cut")])];
        let edit = level(&items, &[0]);
        assert_eq!(names(&edit), ["Undo", "Inside", "Cut"]);
        assert_eq!(edit[1].at, [0, 1, 0]);
        assert_eq!(edit[1].path, ["Edit"]);
        assert_eq!(texts(&everything(&items)), ["Edit", "Edit › Undo", "Edit › Inside", "Edit › Cut"]);
    }

    #[test]
    fn everything_searchable_in_menu_order() {
        assert_eq!(
            texts(&everything(&menu())),
            [
                "Safari",
                "Safari › About Safari",
                "Safari › Settings…",
                "File",
                "File › New Window",
                "File › New Tab",
                "File › Open Recent",
                "View",
                "View › Show Tab Overview",
                "View › Zoom",
                "View › Zoom › Zoom In",
                "View › Zoom › Actual Size",
            ]
        );
    }

    #[test]
    fn shortcuts_in_mac_form() {
        assert_eq!(shown_shortcut("Super+N"), "⌘N");
        assert_eq!(shown_shortcut("Shift+Super+N"), "⇧⌘N");
        assert_eq!(shown_shortcut("Control+Alt+Shift+Super+Q"), "⌃⌥⇧⌘Q");
        assert_eq!(shown_shortcut("Alt+Super+I"), "⌥⌘I");
        assert_eq!(shown_shortcut("Control+F"), "⌃F");
        assert_eq!(shown_shortcut("Super++"), "⌘+");
        assert_eq!(shown_shortcut("Super+Backspace"), "⌘⌫");
        assert_eq!(shown_shortcut("Super+Return"), "⌘↩");
        assert_eq!(shown_shortcut("Super+\t"), "⌘⇥");
        assert_eq!(shown_shortcut(""), "");
    }

    #[test]
    fn private_shortcut_characters_are_not_shown() {
        assert_eq!(shown_shortcut("Super+\u{f700}"), "");
        assert_eq!(shown_shortcut("Super+\u{3}"), "");
    }

    #[test]
    fn nothing_typed_shows_the_menu_shown() {
        let items = menu();
        assert_eq!(names(&rows_for(&items, &[], "")), ["Safari", "File", "View"]);
        assert_eq!(names(&rows_for(&items, &[2, 2], "   ")), ["Zoom In", "Actual Size"]);
    }

    #[test]
    fn typing_searches_every_level() {
        let items = menu();
        // From inside a submenu too.
        assert_eq!(texts(&rows_for(&items, &[0], "zoom IN")), ["View › Zoom › Zoom In"]);
        assert_eq!(texts(&rows_for(&items, &[], "file tab")), ["File › New Tab"]);
        assert_eq!(texts(&rows_for(&items, &[], "view size")), ["View › Zoom › Actual Size"]);
        assert!(rows_for(&items, &[], "file zoom").is_empty());
        // Disabled items and what's in disabled submenus can't be found.
        assert!(rows_for(&items, &[], "close").is_empty());
        assert!(rows_for(&items, &[], "unicode").is_empty());
        assert!(rows_for(&items, &[], "encoding").is_empty());
    }

    #[test]
    fn names_starting_with_the_text_come_first() {
        let items = menu();
        let found = |typed| texts(&rows_for(&items, &[], typed));
        // "Show Tab Overview" and "New Tab" only contain it; nothing starts with it.
        assert_eq!(found("tab"), ["File › New Tab", "View › Show Tab Overview"]);
        // The submenu and its first item start with it; the other matches through its path.
        assert_eq!(found("zoom"), ["View › Zoom", "View › Zoom › Zoom In", "View › Zoom › Actual Size"]);
        assert_eq!(found("new"), ["File › New Window", "File › New Tab"]);
        assert_eq!(found("new t"), ["File › New Tab"]);
        assert_eq!(found("open"), ["File › Open Recent"]);
        assert_eq!(found("S"), [
            "Safari",
            "Safari › Settings…",
            "View › Show Tab Overview",
            "Safari › About Safari",
            "View › Zoom › Actual Size",
        ]);
    }

    #[test]
    fn where_enter_leads() {
        let items = menu();
        let top = level(&items, &[]);
        assert_eq!(choice(top.get(1)), Choice::Open(vec![1]));
        let file = level(&items, &[1]);
        assert_eq!(choice(file.get(1)), Choice::Run(file[1].clone()));
        assert_eq!(file[1].id, 7);
        // Into an empty submenu: it says there's nothing in it.
        assert_eq!(choice(file.get(2)), Choice::Open(vec![1, 2]));
        assert_eq!(choice(file.get(3)), Choice::Nothing);
        assert_eq!(choice(file.get(4)), Choice::Nothing);
        assert_eq!(choice(file.get(9)), Choice::Nothing);
        assert_eq!(choice(level(&items, &[2]).get(1)), Choice::Nothing);
        // A submenu found by searching opens where it is.
        let found = rows_for(&items, &[], "zoom");
        assert_eq!(choice(found.first()), Choice::Open(vec![2, 2]));
        assert_eq!(choice(found.get(1)), Choice::Run(found[1].clone()));
    }

    #[test]
    fn going_back_up() {
        let items = menu();
        assert_eq!(up(&items, &[2, 2]), [2]);
        assert_eq!(up(&items, &[2]), Vec::<usize>::new());
        // Past an untitled submenu, which was never a level of its own.
        let nested = vec![sub(1, "Edit", vec![sub(2, "", vec![sub(3, "Find", vec![item(4, "Find…")])])])];
        assert_eq!(names(&level(&nested, &[0])), ["Find"]);
        assert_eq!(level(&nested, &[0])[0].at, [0, 0, 0]);
        assert_eq!(trail(&nested, &[0, 0, 0]), ["Edit", "Find"]);
        assert_eq!(up(&nested, &[0, 0, 0]), [0]);
    }

    #[test]
    fn where_it_is() {
        let items = menu();
        assert!(trail(&items, &[]).is_empty());
        assert_eq!(trail(&items, &[1, 2]), ["File", "Open Recent"]);
    }

    #[test]
    fn the_same_submenu_in_a_fresh_menu() {
        let items = menu();
        assert_eq!(find_level(&items, &["View".into(), "Zoom".into()]), Some(vec![2, 2]));
        assert_eq!(find_level(&items, &[]), Some(vec![]));
        assert_eq!(find_level(&items, &["View".into(), "Text Encoding".into()]), None);
        assert_eq!(find_level(&items, &["Window".into()]), None);
    }

    #[test]
    fn the_keyboard_skips_what_cant_be_chosen() {
        let items = vec![sub(
            1,
            "Edit",
            vec![Item { enabled: false, ..item(2, "Undo") }, item(3, "Cut"), sep(4), Item { enabled: false, ..item(5, "Paste") }, item(6, "Select All")],
        )];
        let rows = level(&items, &[0]);
        assert_eq!(names(&rows), ["Undo", "Cut", "---", "Paste", "Select All"]);
        assert_eq!(first_choosable(&rows), 1);
        assert_eq!(step(&rows, 1, 1), 4);
        assert_eq!(step(&rows, 4, -1), 1);
        assert_eq!(step(&rows, 4, 1), 4);
        assert_eq!(step(&rows, 1, -1), 1);
        assert_eq!(step(&rows, 1, 10), 4);
        assert_eq!(step(&rows, 4, -10), 1);
        assert_eq!(step(&[], 0, 1), 0);
        let none = level(&[sub(1, "Edit", vec![Item { enabled: false, ..item(2, "Undo") }])], &[0]);
        assert_eq!(step(&none, 0, 1), 0);
        assert_eq!(first_choosable(&none), 0);
    }

    #[test]
    fn nothing_is_highlighted_until_a_key() {
        let items = vec![sub(1, "Edit", vec![Item { enabled: false, ..item(2, "Undo") }, item(3, "Cut"), sep(4), item(5, "Paste"), Item { enabled: false, ..item(6, "Delete") }])];
        let rows = level(&items, &[0]);
        // As it opens: none, unless one is kept.
        assert_eq!(highlight(&rows, None, false), None);
        assert_eq!(highlight(&rows, Some(3), false), Some(3));
        // Once the keyboard has been used (a search: the best match).
        assert_eq!(highlight(&rows, None, true), Some(1));
        let none = level(&[sub(1, "Edit", vec![Item { enabled: false, ..item(2, "Undo") }])], &[0]);
        assert_eq!(highlight(&none, None, true), None);
        assert_eq!(highlight(&[], None, true), None);
        // The first arrow key: Down to the first row that can be chosen, Up to the last.
        assert_eq!(step_from(&rows, None, 1), Some(1));
        assert_eq!(step_from(&rows, None, -1), Some(3));
        assert_eq!(step_from(&rows, Some(1), 1), Some(3));
        assert_eq!(step_from(&none, None, 1), None);
        assert_eq!(step_from(&[], None, -1), None);
    }

    #[test]
    fn an_overlay_where_the_desktop_draws_one() {
        assert!(overlay_here("KDE", false));
        assert!(!overlay_here("ubuntu:GNOME", false));
        assert!(overlay_here("X-Cinnamon", false));
        assert!(!overlay_here("Hyprland", true));
        // Hyprland started from a session that says KDE, say.
        assert!(!overlay_here("KDE", true));
        assert!(!overlay_here("sway", false));
        assert!(!overlay_here("niri", false));
        assert!(!overlay_here("", false));
    }
}
