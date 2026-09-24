//! Shared clipboard with the Mac: text, images and files, both ways.
//!
//! Wire format (on the clipboard stream): u16 n, then n × (str kind, str name, bytes data), with
//! kind `text/plain;charset=utf-8`, `image/png` or `file`.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    io::Read,
    path::PathBuf,
    sync::Arc,
};

use anyhow::Result;

use crate::proto::{t, R, W};

pub const TEXT: &str = "text/plain;charset=utf-8";
pub const PNG: &str = "image/png";
pub const FILE: &str = "file";

/// Largest total we copy across (files included).
pub const MAX_BYTES: usize = 512 << 20;

#[derive(Clone, Debug)]
pub struct ClipItem {
    pub kind: String,
    pub name: String,
    pub data: Vec<u8>,
}

pub fn encode(items: &[ClipItem]) -> Vec<u8> {
    let mut w = W::new(t::CLIPBOARD).u16(items.len() as u16);
    for it in items {
        w = w.str(&it.kind).str(&it.name).bytes(&it.data);
    }
    w.finish()
}

pub fn decode(body: &[u8]) -> Result<Vec<ClipItem>> {
    let mut r = R::new(body);
    let n = r.u16()?;
    (0..n).map(|_| Ok(ClipItem { kind: r.str()?, name: r.str()?, data: r.bytes()?.to_vec() })).collect()
}

pub fn fingerprint(items: &[ClipItem]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for it in items {
        it.kind.hash(&mut h);
        it.name.hash(&mut h);
        it.data.hash(&mut h);
    }
    h.finish()
}

/// What we offer on the Linux clipboard for items from the Mac: MIME type → bytes.
/// Files are saved to ~/Downloads/From Mac first and offered as URIs (what Dolphin pastes).
pub fn offers_for(items: &[ClipItem]) -> HashMap<String, Arc<Vec<u8>>> {
    let mut out: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let files: Vec<PathBuf> = items.iter().filter(|i| i.kind == FILE).filter_map(save_from_mac).collect();
    if !files.is_empty() {
        let uris: Vec<String> = files.iter().map(|p| format!("file://{}", uri_escape(&p.to_string_lossy()))).collect();
        out.insert("text/uri-list".into(), Arc::new(format!("{}\r\n", uris.join("\r\n")).into_bytes()));
        out.insert("x-special/gnome-copied-files".into(), Arc::new(format!("copy\n{}", uris.join("\n")).into_bytes()));
        let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        out.insert(TEXT.into(), Arc::new(paths.join("\n").into_bytes()));
    }
    if let Some(png) = items.iter().find(|i| i.kind == PNG) {
        out.insert(PNG.into(), Arc::new(png.data.clone()));
    }
    if let Some(text) = items.iter().find(|i| i.kind == TEXT) {
        let data = Arc::new(text.data.clone());
        for mime in [TEXT, "text/plain", "UTF8_STRING", "STRING", "TEXT"] {
            out.entry(mime.into()).or_insert_with(|| data.clone());
        }
    }
    out
}

fn downloads() -> PathBuf {
    // Honour the XDG downloads dir when set, else ~/Downloads.
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    std::fs::read_to_string(home.join(".config/user-dirs.dirs"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR=")).map(|v| {
                PathBuf::from(v.trim_matches('"').replace("$HOME", &home.to_string_lossy()))
            })
        })
        .unwrap_or_else(|| home.join("Downloads"))
}

fn save_from_mac(item: &ClipItem) -> Option<PathBuf> {
    let dir = downloads().join("From Mac");
    std::fs::create_dir_all(&dir).ok()?;
    let name = std::path::Path::new(&item.name).file_name()?.to_string_lossy().into_owned();
    let mut path = dir.join(&name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} {n}{ext}"));
        n += 1;
    }
    std::fs::write(&path, &item.data).ok()?;
    Some(path)
}

fn uri_escape(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn uri_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The best Linux clipboard format we can send, in order of preference.
pub fn pick_mime(offered: &[String]) -> Option<String> {
    for want in ["text/uri-list", PNG, TEXT, "UTF8_STRING", "text/plain"] {
        if offered.iter().any(|m| m == want) {
            return Some(want.to_string());
        }
    }
    offered.iter().find(|m| m.starts_with("image/")).cloned()
}

/// Turns the bytes read for `mime` into items for the Mac (reads the files of a URI list).
pub fn items_from(mime: &str, data: Vec<u8>) -> Vec<ClipItem> {
    if mime == "text/uri-list" {
        let mut total = 0;
        let mut items = Vec::new();
        for line in String::from_utf8_lossy(&data).lines() {
            let Some(path) = line.trim().strip_prefix("file://") else { continue };
            let path = uri_unescape(path);
            let p = std::path::Path::new(&path);
            if p.is_dir() {
                log::warn!("clipboard: folders aren't copied to the Mac yet ({path})");
                continue;
            }
            let Ok(mut f) = std::fs::File::open(p) else { continue };
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            total += buf.len();
            if total > MAX_BYTES {
                log::warn!("clipboard: files too large to copy to the Mac (limit 512 MB)");
                return Vec::new();
            }
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            items.push(ClipItem { kind: FILE.into(), name, data: buf });
        }
        return items;
    }
    if mime.starts_with("image/") {
        // Sent as the image kind; the Mac decodes any common format (PNG, JPEG, …).
        return vec![ClipItem { kind: PNG.into(), name: String::new(), data }];
    }
    vec![ClipItem { kind: TEXT.into(), name: String::new(), data }]
}
