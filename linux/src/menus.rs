//! The Mac apps' menu bars as native KDE menus: each app's menu is served over D-Bus with the
//! `com.canonical.dbusmenu` interface (what Plasma's Global Menu reads), and each of its windows
//! points KWin at it through the `org_kde_kwin_appmenu` protocol.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use zbus::{blocking, interface, object_server::SignalEmitter};
use zvariant::{OwnedValue, Signature, Str, StructureBuilder, Type, Value};

use crate::{
    net::NetCmd,
    proto::{t, W},
};

/// One item as sent by the Mac (see MenuReader.swift).
#[derive(Deserialize, Debug, Clone)]
pub struct MacItem {
    t: String,
    #[serde(default = "yes")]
    e: bool,
    sep: Option<bool>,
    k: Option<String>,
    m: Option<i32>,
    c: Option<bool>,
    sub: Option<Vec<MacItem>>,
}

fn yes() -> bool {
    true
}

/// A menu item with its D-Bus id and its path on the Mac (for pressing it).
struct Node {
    id: i32,
    item: MacItem,
    children: Vec<Node>,
}

struct Model {
    revision: u32,
    root: Node,
    by_id: HashMap<i32, Vec<u16>>,
    last_refresh: Instant,
}

fn build(items: &[MacItem], path: &[u16], next: &mut i32, by_id: &mut HashMap<i32, Vec<u16>>) -> Vec<Node> {
    items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let mut p = path.to_vec();
            p.push(i as u16);
            let id = *next;
            *next += 1;
            by_id.insert(id, p.clone());
            let children = it.sub.as_deref().map(|s| build(s, &p, next, by_id)).unwrap_or_default();
            Node { id, item: it.clone(), children }
        })
        .collect()
}

/// A dbusmenu layout node: `(ia{sv}av)`, children being variants of the same.
#[derive(Serialize)]
struct Layout(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

impl Type for Layout {
    const SIGNATURE: &'static Signature = &zvariant::signature!("(ia{sv}av)");
}

fn ov<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    v.into().try_to_owned().expect("plain value")
}

/// Mac key equivalent → dbusmenu shortcut (`aas`), in the keys you press here: ⌘ is Meta.
fn shortcut(it: &MacItem) -> Option<Vec<Vec<String>>> {
    let key = it.k.as_ref()?;
    let m = it.m.unwrap_or(0);
    let mut combo = Vec::new();
    if m & 4 != 0 {
        combo.push("Control".to_string());
    }
    if m & 2 != 0 {
        combo.push("Alt".to_string());
    }
    if m & 1 != 0 {
        combo.push("Shift".to_string());
    }
    if m & 8 == 0 {
        combo.push("Super".to_string());
    }
    combo.push(match key.as_str() {
        "\u{8}" | "\u{7f}" => "Backspace".into(),
        "\u{1b}" => "Escape".into(),
        "\r" => "Return".into(),
        " " => "Space".into(),
        k => k.to_uppercase(),
    });
    Some(vec![combo])
}

fn props(n: &Node) -> HashMap<String, OwnedValue> {
    let mut p = HashMap::new();
    if n.id == 0 {
        p.insert("children-display".into(), ov("submenu"));
        return p;
    }
    if n.item.sep == Some(true) {
        p.insert("type".into(), ov("separator"));
        return p;
    }
    // "_" marks a mnemonic in dbusmenu labels.
    p.insert("label".into(), ov(Str::from(n.item.t.replace('_', "__"))));
    if !n.item.e {
        p.insert("enabled".into(), ov(false));
    }
    if !n.children.is_empty() {
        p.insert("children-display".into(), ov("submenu"));
    }
    if let Some(sc) = shortcut(&n.item) {
        p.insert("shortcut".into(), ov(Value::from(sc)));
    }
    if let Some(c) = n.item.c {
        p.insert("toggle-type".into(), ov("checkmark"));
        p.insert("toggle-state".into(), ov(c as i32));
    }
    p
}

fn layout(n: &Node, depth: i32) -> Layout {
    let children = if depth == 0 {
        Vec::new()
    } else {
        n.children
            .iter()
            .map(|c| {
                let l = layout(c, depth - 1);
                let s = StructureBuilder::new().add_field(l.0).add_field(l.1).add_field(l.2).build().expect("structure");
                ov(Value::Structure(s))
            })
            .collect()
    };
    Layout(n.id, props(n), children)
}

fn find(n: &Node, id: i32) -> Option<&Node> {
    if n.id == id {
        return Some(n);
    }
    n.children.iter().find_map(|c| find(c, id))
}

type Models = Arc<Mutex<HashMap<i32, Model>>>;

/// `com.canonical.dbusmenu` for one Mac app.
struct MenuIface {
    pid: i32,
    models: Models,
    net: UnboundedSender<NetCmd>,
}

#[interface(name = "com.canonical.dbusmenu")]
impl MenuIface {
    #[zbus(name = "GetLayout")]
    fn get_layout(&self, parent: i32, depth: i32, _props: Vec<String>) -> (u32, Layout) {
        let models = self.models.lock().unwrap();
        let Some(m) = models.get(&self.pid) else { return (0, Layout(0, HashMap::new(), Vec::new())) };
        let node = find(&m.root, parent).unwrap_or(&m.root);
        (m.revision, layout(node, depth))
    }

    #[zbus(name = "GetGroupProperties")]
    fn get_group_properties(&self, ids: Vec<i32>, _props: Vec<String>) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let models = self.models.lock().unwrap();
        let Some(m) = models.get(&self.pid) else { return Vec::new() };
        ids.iter().filter_map(|id| find(&m.root, *id).map(|n| (*id, props(n)))).collect()
    }

    #[zbus(name = "GetProperty")]
    fn get_property(&self, id: i32, name: String) -> OwnedValue {
        let models = self.models.lock().unwrap();
        models
            .get(&self.pid)
            .and_then(|m| find(&m.root, id))
            .and_then(|n| props(n).remove(&name))
            .unwrap_or_else(|| ov(""))
    }

    #[zbus(name = "Event")]
    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        let path = self.models.lock().unwrap().get(&self.pid).and_then(|m| m.by_id.get(&id).cloned());
        if let Some(path) = path {
            let mut w = W::new(t::MENU_ACTIVATE).i32(self.pid).u16(path.len() as u16);
            for p in path {
                w = w.u16(p);
            }
            let _ = self.net.send(NetCmd::Control(w.finish()));
        }
    }

    #[zbus(name = "EventGroup")]
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, ev, data, ts) in events {
            self.event(id, ev, data, ts);
        }
        Vec::new()
    }

    /// A menu is about to open: ask the Mac for fresh state (enabled items, checkmarks).
    #[zbus(name = "AboutToShow")]
    fn about_to_show(&self, _id: i32) -> bool {
        let mut models = self.models.lock().unwrap();
        if let Some(m) = models.get_mut(&self.pid) {
            if m.last_refresh.elapsed() > Duration::from_millis(800) {
                m.last_refresh = Instant::now();
                let _ = self.net.send(NetCmd::Control(W::new(t::MENU_REFRESH).i32(self.pid).finish()));
            }
        }
        false
    }

    #[zbus(name = "AboutToShowGroup")]
    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        if let Some(id) = ids.first() {
            self.about_to_show(*id);
        }
        (Vec::new(), Vec::new())
    }

    #[zbus(signal, name = "LayoutUpdated")]
    async fn layout_updated(emitter: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;

    #[zbus(property, name = "Version")]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property, name = "TextDirection")]
    fn text_direction(&self) -> String {
        "ltr".into()
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> String {
        "normal".into()
    }

    #[zbus(property, name = "IconThemePath")]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Owns the D-Bus connection and the per-app menu models.
pub struct Menus {
    conn: blocking::Connection,
    models: Models,
    net: UnboundedSender<NetCmd>,
    registered: std::collections::HashSet<i32>,
}

impl Menus {
    pub fn new(net: UnboundedSender<NetCmd>) -> zbus::Result<Self> {
        Ok(Menus { conn: blocking::Connection::session()?, models: Default::default(), net, registered: Default::default() })
    }

    /// Our unique bus name, which KWin is given along with the object path.
    pub fn service(&self) -> String {
        self.conn.unique_name().map(|n| n.to_string()).unwrap_or_default()
    }

    pub fn object_path(pid: i32) -> String {
        format!("/Sweeflow/Menu/{pid}")
    }

    /// New menu tree for an app (JSON from the Mac).
    pub fn update(&mut self, pid: i32, json: &[u8]) {
        let items: Vec<MacItem> = match serde_json::from_slice(json) {
            Ok(i) => i,
            Err(e) => {
                log::warn!("bad menu for {pid}: {e}");
                return;
            }
        };
        let mut by_id = HashMap::new();
        let mut next = 1;
        let children = build(&items, &[], &mut next, &mut by_id);
        let root = Node {
            id: 0,
            item: MacItem { t: String::new(), e: true, sep: None, k: None, m: None, c: None, sub: None },
            children,
        };
        let revision = {
            let mut models = self.models.lock().unwrap();
            let revision = models.get(&pid).map_or(1, |m| m.revision + 1);
            models.insert(pid, Model { revision, root, by_id, last_refresh: Instant::now() });
            revision
        };
        let path = Self::object_path(pid);
        if self.registered.insert(pid) {
            let iface = MenuIface { pid, models: self.models.clone(), net: self.net.clone() };
            if let Err(e) = self.conn.object_server().at(path.as_str(), iface) {
                log::warn!("menu for {pid}: {e}");
            }
            log::info!("menus for pid {pid}: {} top-level items", items.len());
        } else if let Ok(iface) = self.conn.object_server().interface::<_, MenuIface>(path.as_str()) {
            let emitter = iface.signal_emitter().clone();
            let _ = zbus::block_on(MenuIface::layout_updated(&emitter, revision, 0));
        }
    }
}
