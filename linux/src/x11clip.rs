//! The clipboard on X11 (the X11 frontend, x11ui.rs): what's copied on the Mac is offered as the
//! CLIPBOARD selection here, and what's copied here goes to the Mac. On its own thread, with its
//! own X connection and an unmapped window that owns the selection.
//!
//! Large items (a picture, a file list) go in parts (the ICCCM's INCR); changes made by other
//! apps are seen through XFixes.

use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use tokio::sync::mpsc::UnboundedSender;
use x11rb::{
    connection::Connection,
    protocol::{
        xfixes::{self, ConnectionExt as _},
        xproto::{self, AtomEnum, ConnectionExt as _, EventMask, PropMode, Window},
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::net::NetCmd;

/// Offers for the CLIPBOARD selection (MIME type → bytes), from the Mac.
pub type Offers = HashMap<String, Arc<Vec<u8>>>;

/// Hands the X11 clipboard thread what the Mac copied.
#[derive(Clone)]
pub struct Clipboard(mpsc::Sender<Offers>);

impl Clipboard {
    pub fn set(&self, offers: Offers) {
        let _ = self.0.send(offers);
    }
}

/// Starts the clipboard thread; None when there's no X display to talk to.
pub fn start(net: UnboundedSender<NetCmd>) -> Option<Clipboard> {
    let (tx, rx) = mpsc::channel();
    let (conn, screen) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("clipboard: no X display ({e})");
            return None;
        }
    };
    std::thread::Builder::new()
        .name("x11-clipboard".into())
        .spawn(move || {
            if let Err(e) = run(conn, screen, rx, net) {
                log::warn!("clipboard: {e:#}");
            }
        })
        .ok()?;
    Some(Clipboard(tx))
}

struct Atoms {
    clipboard: u32,
    targets: u32,
    incr: u32,
    property: u32,
    atom: u32,
}

/// A piece at a time for big transfers (well under the server's request size).
const CHUNK: usize = 256 * 1024;

/// A transfer in parts to a requestor (INCR): what's left to send, and where.
struct Outgoing {
    requestor: Window,
    property: u32,
    target: u32,
    data: Arc<Vec<u8>>,
    sent: usize,
    started: Instant,
}

fn run(conn: RustConnection, screen: usize, rx: mpsc::Receiver<Offers>, net: UnboundedSender<NetCmd>) -> Result<()> {
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let intern = |n: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, n)?.reply()?.atom) };
    let a = Atoms {
        clipboard: intern(b"CLIPBOARD")?,
        targets: intern(b"TARGETS")?,
        incr: intern(b"INCR")?,
        property: intern(b"SWEEFLOW_CLIP")?,
        atom: AtomEnum::ATOM.into(),
    };
    // Told whenever the CLIPBOARD changes hands.
    conn.xfixes_query_version(5, 0)?.reply().context("XFixes")?;
    conn.xfixes_select_selection_input(win, a.clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER)?;
    conn.flush()?;

    let mut offers: Offers = HashMap::new();
    let mut atom_names: HashMap<u32, String> = HashMap::new();
    let mut outgoing: Vec<Outgoing> = Vec::new();
    // Reading the clipboard another app owns: the type asked for, and the parts so far.
    let mut reading: Option<(String, Option<Vec<u8>>)> = None;
    let mut name_of = |id: u32, conn: &RustConnection| -> String {
        atom_names
            .entry(id)
            .or_insert_with(|| conn.get_atom_name(id).ok().and_then(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.name).into_owned()).unwrap_or_default())
            .clone()
    };

    log::info!("clipboard: X11 CLIPBOARD shared with the Mac");
    loop {
        // New offers from the Mac: own the selection with them.
        while let Ok(o) = rx.try_recv() {
            if o.is_empty() {
                continue;
            }
            offers = o;
            conn.set_selection_owner(win, a.clipboard, x11rb::CURRENT_TIME)?;
            conn.flush()?;
        }
        let Some(event) = conn.poll_for_event()? else {
            std::thread::sleep(Duration::from_millis(15));
            outgoing.retain(|o| o.started.elapsed() < Duration::from_secs(30));
            continue;
        };
        match event {
            Event::SelectionRequest(req) => {
                let property = if req.property == x11rb::NONE { req.target } else { req.property };
                let mut ok = true;
                if req.target == a.targets {
                    let mut atoms = vec![a.targets];
                    for m in offers.keys() {
                        atoms.push(conn.intern_atom(false, m.as_bytes())?.reply()?.atom);
                    }
                    conn.change_property32(PropMode::REPLACE, req.requestor, property, a.atom, &atoms)?;
                } else if let Some(data) = offers.get(&name_of(req.target, &conn)).cloned() {
                    if data.len() > CHUNK {
                        // In parts: INCR with the size, then a piece each time the requestor
                        // deletes the property.
                        conn.change_window_attributes(req.requestor, &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
                        conn.change_property32(PropMode::REPLACE, req.requestor, property, a.incr, &[data.len() as u32])?;
                        outgoing.push(Outgoing { requestor: req.requestor, property, target: req.target, data, sent: 0, started: Instant::now() });
                    } else {
                        conn.change_property8(PropMode::REPLACE, req.requestor, property, req.target, &data)?;
                    }
                } else {
                    ok = false;
                }
                let notify = xproto::SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: if ok { property } else { x11rb::NONE },
                };
                conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
                conn.flush()?;
            }
            Event::PropertyNotify(ev) if ev.state == xproto::Property::DELETE => {
                // A requestor took a part: the next one (an empty one ends it).
                if let Some(i) = outgoing.iter().position(|o| o.requestor == ev.window && o.property == ev.atom) {
                    let o = &mut outgoing[i];
                    let end = (o.sent + CHUNK).min(o.data.len());
                    conn.change_property8(PropMode::REPLACE, o.requestor, o.property, o.target, &o.data[o.sent..end])?;
                    let done = o.sent == end;
                    o.sent = end;
                    if done {
                        outgoing.remove(i);
                    }
                    conn.flush()?;
                }
            }
            Event::SelectionClear(_) => {
                offers.clear();
            }
            Event::XfixesSelectionNotify(ev) if ev.owner != win && ev.owner != x11rb::NONE => {
                log::debug!("clipboard: another app copied (owner {:#x})", ev.owner);
                // Another app copied something: what it offers, then the best of it.
                reading = None;
                conn.convert_selection(win, a.clipboard, a.targets, a.property, x11rb::CURRENT_TIME)?;
                conn.flush()?;
            }
            Event::SelectionNotify(ev) if ev.requestor == win && ev.property != x11rb::NONE => {
                let reply = conn.get_property(true, win, a.property, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
                log::debug!("clipboard: got {} ({} bytes, format {})", name_of(ev.target, &conn), reply.value.len(), reply.format);
                if ev.target == a.targets {
                    let mimes: Vec<String> = reply.value32().map(|v| v.collect::<Vec<_>>()).unwrap_or_default().into_iter().map(|t| name_of(t, &conn)).collect();
                    if let Some(mime) = crate::clipboard::pick_mime(&mimes) {
                        let target = conn.intern_atom(false, mime.as_bytes())?.reply()?.atom;
                        reading = Some((mime, None));
                        conn.convert_selection(win, a.clipboard, target, a.property, x11rb::CURRENT_TIME)?;
                    }
                } else if reply.type_ == a.incr {
                    // It comes in parts: each arrives as a new value of the property.
                    if let Some((_, parts)) = reading.as_mut() {
                        *parts = Some(Vec::new());
                    }
                } else if let Some((mime, None)) = reading.take() {
                    send(&net, &mime, reply.value);
                }
                conn.flush()?;
            }
            Event::PropertyNotify(ev) if ev.window == win && ev.atom == a.property && ev.state == xproto::Property::NEW_VALUE => {
                if let Some((_, Some(_))) = reading.as_ref() {
                    let reply = conn.get_property(true, win, a.property, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
                    if reply.value.is_empty() {
                        if let Some((mime, Some(all))) = reading.take() {
                            send(&net, &mime, all);
                        }
                    } else if let Some((_, Some(all))) = reading.as_mut() {
                        if all.len() + reply.value.len() <= crate::clipboard::MAX_BYTES {
                            all.extend_from_slice(&reply.value);
                        }
                    }
                    conn.flush()?;
                }
            }
            _ => {}
        }
    }
}

fn send(net: &UnboundedSender<NetCmd>, mime: &str, data: Vec<u8>) {
    let mime = mime.to_string();
    let net = net.clone();
    // Reading a file list reads the files: not on the X thread.
    std::thread::spawn(move || {
        let items = crate::clipboard::items_from(&mime, data);
        if !items.is_empty() {
            log::debug!("clipboard to the Mac: {} item(s) ({mime})", items.len());
            let _ = net.send(NetCmd::Clipboard(items));
        }
    });
}
