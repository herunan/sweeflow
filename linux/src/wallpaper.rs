//! The Linux wallpaper, for the Mac. macOS tints window materials (sidebars, toolbars, glass)
//! with a blurred sample of the desktop wallpaper, not with what's behind the window. Giving the
//! Mac's virtual display the Linux wallpaper makes Mac windows take on the Linux desktop's
//! colours the way they would on the Mac.

use std::{collections::HashMap, path::PathBuf};

use sha2::{Digest, Sha256};

static SCREEN_ASPECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_screen_aspect(a: f64) {
    SCREEN_ASPECT.store(a.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

pub fn screen_aspect() -> f64 {
    let a = f64::from_bits(SCREEN_ASPECT.load(std::sync::atomic::Ordering::Relaxed));
    if a > 0.0 { a } else { 16.0 / 9.0 }
}

/// The wallpaper image of the first screen (a file; for wallpaper packages, the variant for the
/// colour scheme closest to the screen's shape).
pub fn current(screen_aspect: f64) -> Option<PathBuf> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let reply = conn
        .call_method(Some("org.kde.plasmashell"), "/PlasmaShell", Some("org.kde.PlasmaShell"), "wallpaper", &(0u32,))
        .ok()?;
    let config: HashMap<String, zvariant::OwnedValue> = reply.body().deserialize().ok()?;
    let image: String = config.get("Image").and_then(|v| String::try_from(v.clone()).ok())?;
    let path = PathBuf::from(image.strip_prefix("file://").unwrap_or(&image));
    if path.is_file() {
        return Some(path);
    }
    // A wallpaper package: contents/images[_dark]/WIDTHxHEIGHT.ext
    let dark = std::fs::read_to_string(dirs_config().join("kdeglobals"))
        .map(|s| s.lines().any(|l| (l.starts_with("LookAndFeelPackage=") || l.starts_with("ColorScheme=")) && l.to_lowercase().contains("dark")))
        .unwrap_or(false);
    let variants = if dark { ["contents/images_dark", "contents/images"] } else { ["contents/images", "contents/images_dark"] };
    for v in variants {
        let Ok(entries) = std::fs::read_dir(path.join(v)) else { continue };
        let best = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let (w, h) = name.split('.').next()?.split_once('x')?;
                let (w, h): (f64, f64) = (w.parse().ok()?, h.parse().ok()?);
                // Closest shape; among equals, the bigger one.
                Some((((w / h) - screen_aspect).abs(), -w, e.path()))
            })
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((_, _, p)) = best {
            return Some(p);
        }
    }
    None
}

fn dirs_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

/// The file's bytes and a hash of them.
pub fn load(path: &std::path::Path) -> Option<(String, Vec<u8>)> {
    let data = std::fs::read(path).ok()?;
    let hash = hex::encode(Sha256::digest(&data));
    Some((hash, data))
}

/// Bumps whenever Plasma saves its desktop config (where the wallpaper choice lives) or the
/// colour scheme (light/dark wallpaper variants) changes. One watcher for the whole run.
pub fn changes() -> tokio::sync::watch::Receiver<u64> {
    static TX: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, _) = tokio::sync::watch::channel(0u64);
        let sender = tx.clone();
        std::thread::Builder::new()
            .name("wallpaper-watch".into())
            .spawn(move || {
                if let Err(e) = watch_config(&sender) {
                    log::warn!("wallpaper changes won't be noticed right away: {e}");
                }
            })
            .ok();
        tx
    })
    .subscribe()
}

fn watch_config(tx: &tokio::sync::watch::Sender<u64>) -> std::io::Result<()> {
    use inotify::{Inotify, WatchMask};
    let mut inotify = Inotify::init()?;
    // The directory, not the files: KDE saves by writing a new file and renaming it over.
    inotify.watches().add(dirs_config(), WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE)?;
    let mut buf = [0u8; 4096];
    loop {
        let events = inotify.read_events_blocking(&mut buf)?;
        let relevant = events.into_iter().any(|e| {
            e.name.is_some_and(|n| {
                let n = n.to_string_lossy();
                n == "plasma-org.kde.plasma.desktop-appletsrc" || n == "kdeglobals"
            })
        });
        if relevant {
            tx.send_modify(|n| *n += 1);
        }
    }
}
