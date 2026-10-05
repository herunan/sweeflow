//! Shared clipboard with the Mac: text, images and files, both ways.
//!
//! Wire format (on the clipboard stream): u16 n, then n × (str kind, str name, bytes data), with
//! kind `text/plain;charset=utf-8`, `image/png` or `file`.

use std::{
    collections::HashMap,
    io::Read,
    path::PathBuf,
    sync::Arc,
};

use anyhow::Result;

use crate::proto::{t, R, W};

pub const TEXT: &str = "text/plain;charset=utf-8";
pub const PNG: &str = "image/png";
pub const FILE: &str = "file";
/// A web link (drops only).
pub const URL: &str = "url";

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

/// What we offer on the Linux clipboard (or a drag) for items from the Mac: MIME type → bytes.
/// Files live in a private cache under `~/.cache/sweeflow/<area>` (like a clipboard would hold
/// them) and are offered as URIs, so they only appear where you paste or drop them.
pub fn offers_for(items: &[ClipItem], area: &str) -> HashMap<String, Arc<Vec<u8>>> {
    let mut out: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let has_files = items.iter().any(|i| i.kind == FILE);
    let dir = if has_files { fresh_cache_dir(area) } else { None };
    let files: Vec<PathBuf> =
        items.iter().filter(|i| i.kind == FILE).filter_map(|i| dir.as_ref().and_then(|d| save_to(d, i))).collect();
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

/// ~/.cache/sweeflow/<area>/<n>: a new directory per copy or drag. Older ones are removed once
/// they're 10 minutes old, so a paste or drop that's still copying from one isn't cut short.
fn fresh_cache_dir(area: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("sweeflow")
        .join(area);
    for e in std::fs::read_dir(&base).into_iter().flatten().flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if old.is_some_and(|age| age > std::time::Duration::from_secs(600)) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    let dir = base.join(stamp.to_string());
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn save_to(dir: &std::path::Path, item: &ClipItem) -> Option<PathBuf> {
    let name = std::path::Path::new(&item.name).file_name()?.to_string_lossy().into_owned();
    let path = dir.join(name);
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

// --- drops onto Mac windows ----------------------------------------------------------------

const TEXT_MIMES: [&str; 3] = [TEXT, "text/plain", "UTF8_STRING"];

fn best_image(offered: &[String]) -> Option<String> {
    ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp", "image/tiff"]
        .iter()
        .find(|m| offered.iter().any(|o| o == *m))
        .map(|m| m.to_string())
        .or_else(|| offered.iter().find(|m| m.starts_with("image/")).cloned())
}

/// The type to accept when something is dragged over a Mac window, if we can use it at all.
pub fn drop_mime(offered: &[String]) -> Option<String> {
    let has = |m: &str| offered.iter().any(|o| o == m);
    if has("text/uri-list") {
        return Some("text/uri-list".into());
    }
    if let Some(img) = best_image(offered) {
        return Some(img);
    }
    if has("text/x-moz-url") {
        return Some("text/x-moz-url".into());
    }
    TEXT_MIMES.iter().find(|m| has(m)).map(|m| m.to_string())
}

/// Everything worth reading from a drop (decided once the data is in, see `drop_items`).
pub fn drop_reads(offered: &[String]) -> Vec<String> {
    let has = |m: &str| offered.iter().any(|o| o == m);
    let mut out = Vec::new();
    if has("text/uri-list") {
        out.push("text/uri-list".to_string());
    }
    if let Some(img) = best_image(offered) {
        out.push(img);
    }
    if let Some(t) = TEXT_MIMES.iter().find(|m| has(m)) {
        out.push(t.to_string());
    }
    if out.is_empty() && has("text/x-moz-url") {
        out.push("text/x-moz-url".into());
    }
    out
}

/// What to drop on the Mac: local files if there are any, else the image, else links, else text.
pub fn drop_items(data: Vec<(String, Vec<u8>)>) -> Vec<ClipItem> {
    let get = |pred: &dyn Fn(&str) -> bool| data.iter().find(|(m, d)| pred(m) && !d.is_empty()).map(|(_, d)| d.clone());
    let uri_list = get(&|m| m == "text/uri-list").map(|d| String::from_utf8_lossy(&d).into_owned());
    let uris: Vec<String> = uri_list
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    if uris.iter().any(|u| u.starts_with("file://")) {
        let files = items_from("text/uri-list", uri_list.unwrap_or_default().into_bytes());
        if !files.is_empty() {
            return files;
        }
    }
    if let Some((mime, img)) = data.iter().find(|(m, d)| m.starts_with("image/") && !d.is_empty()) {
        // Name it after the link it came from when there is one (…/photo.jpg), else "Image.png".
        let ext = mime.trim_start_matches("image/").replace("jpeg", "jpg");
        let from_url = uris
            .first()
            .and_then(|u| u.split(['?', '#']).next())
            .and_then(|u| u.rsplit('/').next())
            .map(uri_unescape)
            .filter(|n| n.contains('.') && n.len() < 100);
        let name = from_url.unwrap_or_else(|| format!("Image.{ext}"));
        return vec![ClipItem { kind: FILE.into(), name, data: img.clone() }];
    }
    let web: Vec<&String> = uris.iter().filter(|u| u.starts_with("http://") || u.starts_with("https://")).collect();
    if !web.is_empty() {
        return web.iter().map(|u| ClipItem { kind: URL.into(), name: String::new(), data: u.as_bytes().to_vec() }).collect();
    }
    if let Some(moz) = get(&|m| m == "text/x-moz-url") {
        // UTF-16: URL, newline, title.
        let u16s: Vec<u16> = moz.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        if let Some(url) = String::from_utf16_lossy(&u16s).lines().next().filter(|u| u.starts_with("http")) {
            return vec![ClipItem { kind: URL.into(), name: String::new(), data: url.as_bytes().to_vec() }];
        }
    }
    if let Some(text) = get(&|m| TEXT_MIMES.contains(&m)) {
        return vec![ClipItem { kind: TEXT.into(), name: String::new(), data: text }];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_items_survive_the_wire() {
        let items = vec![
            ClipItem { kind: "text/plain;charset=utf-8".into(), name: String::new(), data: "héllo".as_bytes().to_vec() },
            ClipItem { kind: "file".into(), name: "a b.pdf".into(), data: vec![0, 1, 2, 255] },
        ];
        let msg = encode(&items);
        // encode() frames the message (u32 length, type); decode() takes the body.
        let (ty, body) = (msg[4], &msg[5..]);
        assert_eq!(ty, crate::proto::t::CLIPBOARD);
        let back = decode(body).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].data, "héllo".as_bytes());
        assert_eq!(back[1].name, "a b.pdf");
        assert_eq!(back[1].data, vec![0, 1, 2, 255]);
    }

    #[test]
    fn a_truncated_clipboard_message_is_an_error() {
        let msg = encode(&[ClipItem { kind: "file".into(), name: "x".into(), data: vec![1; 100] }]);
        assert!(decode(&msg[5..msg.len() - 10]).is_err());
    }
}
