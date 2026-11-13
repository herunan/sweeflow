//! Client configuration and identity, stored in ~/.config/sweeflow.

use std::{fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};

/// A Mac this computer is paired with.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PairedMac {
    /// What people call it (the Mac's computer name at pairing).
    pub name: String,
    /// Its Bonjour service name (found again by that when its address changes); may be empty.
    pub service: String,
    /// Last known host:port.
    pub host: String,
    /// SHA-256 of its certificate (hex), learned at pairing.
    pub fingerprint: String,
    /// Every address it said it has (network interfaces, Tailscale) and its .local name, when
    /// last connected: tried when `host` stops answering and the network carries no Bonjour
    /// (a virtual machine's network) to say where it went.
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// host:port of the active Mac.
    pub host: String,
    /// SHA-256 of the active Mac's certificate (hex).
    pub host_fingerprint: String,
    /// Bonjour service name of the active Mac (empty: use `host` only).
    pub service: String,
    /// Every paired Mac; the active one is the one with `host_fingerprint`.
    pub macs: Vec<PairedMac>,
    /// Mac points → Linux pixels. 1.0 on a scale-1 monitor.
    pub scale: f64,
    /// Pixels per point the Mac captures at; 0 = the window's native Retina resolution. Higher
    /// than `scale` means the compositor downsamples, which gives sharper text and full-res colour.
    pub capture_scale: f64,
    /// DRM render node of the GPU the compositor uses (decode there for zero-copy).
    pub render_node: String,
    /// Mac windows with square corners (true) or the Mac's rounded ones (false). Not set: what
    /// suits the desktop (see `square_corners`).
    pub square_corners: Option<bool>,
}

/// The Mac (by fingerprint) said where else it can be reached: kept with it. And, if it was
/// reached at `now` rather than its last known address, that becomes its address.
pub fn remember_addresses(fingerprint: &str, addresses: Vec<String>, now: Option<String>) {
    let Ok(mut c) = Config::load_or_default() else { return };
    let Some(m) = c.macs.iter_mut().find(|m| m.fingerprint == fingerprint) else { return };
    let mut changed = false;
    if !addresses.is_empty() && m.addresses != addresses {
        m.addresses = addresses;
        changed = true;
    }
    if let Some(now) = now.filter(|n| *n != m.host) {
        log::info!("{}: its address is now {now} (was {})", m.name, m.host);
        m.host = now.clone();
        if c.host_fingerprint == fingerprint {
            c.host = now;
        }
        changed = true;
    }
    if changed {
        if let Err(e) = c.save() {
            log::warn!("saving config: {e:#}");
        }
    }
}

/// Every place to try for a Mac (host:port), its last known address first.
pub fn places(m: &PairedMac) -> Vec<String> {
    let port = m.host.rsplit_once(':').map_or("44120", |(_, p)| p);
    let mut out = vec![m.host.clone()];
    for a in &m.addresses {
        let p = if a.contains(':') { a.clone() } else { format!("{a}:{port}") };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Whether Mac windows are shown with square corners: the setting, or without one what suits
/// the desktop (square where it frames every window as a plain rectangle, as Omarchy does;
/// rounded elsewhere). Read once, then kept by `set_square_corners`.
static SQUARE_CORNERS: std::sync::LazyLock<std::sync::atomic::AtomicBool> = std::sync::LazyLock::new(|| {
    let set = Config::load_or_default().ok().and_then(|c| c.square_corners);
    std::sync::atomic::AtomicBool::new(set.unwrap_or_else(crate::hyprland::square_windows))
});

pub fn square_corners() -> bool {
    SQUARE_CORNERS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Switches square corners on or off, and saves it. Open windows follow at once.
pub fn set_square_corners(on: bool) {
    if SQUARE_CORNERS.swap(on, std::sync::atomic::Ordering::SeqCst) == on {
        return;
    }
    log::info!("Mac windows: {} corners", if on { "square" } else { "rounded" });
    crate::net::tell_ui(crate::net::UiEvent::CornersSetting);
    std::thread::spawn(|| {
        // One save at a time, each with the setting as it is by then: switched twice in a row,
        // two saves side by side could leave the older choice in the file.
        static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = SAVING.lock().unwrap();
        match Config::load_or_default() {
            Ok(mut c) => {
                c.square_corners = Some(square_corners());
                if let Err(e) = c.save() {
                    log::warn!("saving config: {e:#}");
                }
            }
            Err(e) => log::warn!("config: {e:#}"),
        }
    });
}

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("sweeflow")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: String::new(),
            host_fingerprint: String::new(),
            service: String::new(),
            macs: Vec::new(),
            scale: 1.0,
            capture_scale: 0.0,
            render_node: default_render_node(),
            square_corners: None,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dir().join("config")
    }

    /// The config, or an empty one (no Mac paired yet) if there is none.
    pub fn load_or_default() -> Result<Config> {
        if Self::path().exists() { Self::load_any() } else { Ok(Config::default()) }
    }

    /// The config with an active Mac (what streaming needs).
    pub fn load() -> Result<Config> {
        let c = Self::load_any()
            .with_context(|| format!("no config at {} — pair a Mac with `sweeflow pair` first", Self::path().display()))?;
        if c.host.is_empty() || c.host_fingerprint.is_empty() {
            bail!("no Mac paired yet — run `sweeflow pair`");
        }
        Ok(c)
    }

    fn load_any() -> Result<Config> {
        Self::parse(&fs::read_to_string(Self::path())?)
    }

    /// Top-level `key = value` settings, then one `[mac]` section per paired Mac. The older
    /// format (one Mac as top-level `host`/`host_fingerprint`) reads as a single paired Mac.
    pub fn parse(text: &str) -> Result<Config> {
        let mut c = Config { render_node: String::new(), ..Config::default() };
        let mut active = String::new();
        let mut legacy = (String::new(), String::new());
        let mut cur: Option<PairedMac> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if line == "[mac]" {
                c.macs.extend(cur.take());
                cur = Some(PairedMac::default());
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim().trim_matches('"').to_string();
            if let Some(m) = cur.as_mut() {
                match k.trim() {
                    "name" => m.name = v,
                    "service" => m.service = v,
                    "host" => m.host = v,
                    "fingerprint" => m.fingerprint = v.to_lowercase(),
                    "addresses" => m.addresses = v.split(',').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect(),
                    _ => {}
                }
                continue;
            }
            match k.trim() {
                "active" => active = v.to_lowercase(),
                "host" => legacy.0 = v,
                "host_fingerprint" => legacy.1 = v.to_lowercase(),
                "scale" => c.scale = v.parse().context("scale")?,
                "capture_scale" => c.capture_scale = v.parse().context("capture_scale")?,
                "render_node" => c.render_node = v,
                "square_corners" => c.square_corners = Some(v != "false"),
                _ => {}
            }
        }
        c.macs.extend(cur.take());
        c.macs.retain(|m| !m.host.is_empty() && m.fingerprint.len() == 64);
        if !legacy.0.is_empty() && !legacy.1.is_empty() && !c.macs.iter().any(|m| m.fingerprint == legacy.1) {
            let name = legacy.0.split(':').next().unwrap_or(&legacy.0).trim_end_matches(".local").to_string();
            c.macs.push(PairedMac { name, host: legacy.0, fingerprint: legacy.1.clone(), ..Default::default() });
            if active.is_empty() {
                active = legacy.1;
            }
        }
        if c.render_node.is_empty() {
            c.render_node = "/dev/dri/renderD128".into();
        }
        let pick = c.macs.iter().find(|m| m.fingerprint == active).or(c.macs.first()).cloned();
        if let Some(m) = pick {
            c.set_active(&m);
        }
        Ok(c)
    }

    fn set_active(&mut self, m: &PairedMac) {
        self.host = m.host.clone();
        self.host_fingerprint = m.fingerprint.clone();
        self.service = m.service.clone();
    }

    /// Makes `fingerprint` the Mac to stream from.
    pub fn activate(&mut self, fingerprint: &str) -> bool {
        let Some(m) = self.macs.iter().find(|m| m.fingerprint == fingerprint).cloned() else { return false };
        self.set_active(&m);
        true
    }

    /// Adds (or updates) a paired Mac; `activate` makes it the one to stream from.
    /// Pairing again with a Mac it knows keeps the name given to it here (see tray::rename).
    pub fn add_mac(&mut self, mut m: PairedMac, activate: bool) {
        if let Some(old) = self.macs.iter().find(|o| o.fingerprint == m.fingerprint) {
            m.name = old.name.clone();
            if m.addresses.is_empty() {
                m.addresses = old.addresses.clone();
            }
        }
        self.macs.retain(|o| o.fingerprint != m.fingerprint);
        self.macs.push(m.clone());
        if activate || self.host_fingerprint == m.fingerprint {
            self.set_active(&m);
        }
    }

    /// Forgets a Mac. If it was the active one, another paired Mac (if any) takes over.
    pub fn forget(&mut self, fingerprint: &str) {
        self.macs.retain(|m| m.fingerprint != fingerprint);
        if self.host_fingerprint == fingerprint {
            match self.macs.first().cloned() {
                Some(m) => self.set_active(&m),
                None => {
                    self.host.clear();
                    self.host_fingerprint.clear();
                    self.service.clear();
                }
            }
        }
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "# sweeflow client configuration\nactive = \"{}\"\nscale = {}\n\
             # 0 = capture at the Mac's native Retina resolution (sharpest); 1 = half the bandwidth\n\
             capture_scale = {}\nrender_node = \"{}\"\n",
            self.host_fingerprint, self.scale, self.capture_scale, self.render_node
        );
        if let Some(square) = self.square_corners {
            out += &format!("square_corners = {square}\n");
        }
        for m in &self.macs {
            out += &format!(
                "\n[mac]\nname = \"{}\"\nservice = \"{}\"\nhost = \"{}\"\nfingerprint = \"{}\"\n",
                m.name.replace('"', ""), m.service.replace('"', ""), m.host, m.fingerprint
            );
            if !m.addresses.is_empty() {
                let list: Vec<String> = m.addresses.iter().map(|a| a.replace(['"', ','], "")).collect();
                out += &format!("addresses = \"{}\"\n", list.join(","));
            }
        }
        out
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(dir())?;
        // Written whole, then renamed: the tray and the wizard may both save.
        let tmp = Self::path().with_extension("tmp");
        fs::write(&tmp, self.render())?;
        fs::rename(tmp, Self::path())?;
        Ok(())
    }

}

pub fn client_identity() -> Result<(CertificateDer<'static>, Vec<u8>)> {
    let d = dir();
    let cert_path = d.join("client.der");
    let key_path = d.join("client.key.der");
    if !cert_path.exists() || !key_path.exists() {
        fs::create_dir_all(&d)?;
        let host = std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "linux".into());
        let ck = rcgen::generate_simple_self_signed(vec![format!("sweeflow-{}", host.trim())])?;
        fs::write(&cert_path, ck.cert.der())?;
        fs::write(&key_path, ck.signing_key.serialize_der())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok((CertificateDer::from(fs::read(cert_path)?), fs::read(key_path)?))
}

pub fn client_fingerprint() -> Result<String> {
    let (cert, _) = client_identity()?;
    Ok(hex::encode(Sha256::digest(cert.as_ref())))
}

/// Picks the render node of the GPU driving the compositor: the DRM device KWin reports as
/// "boot_vga", falling back to the first render node.
pub fn default_render_node() -> String {
    let mut nodes: Vec<_> = fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    for n in &nodes {
        let boot_vga = fs::read_to_string(format!("/sys/class/drm/{n}/device/boot_vga")).unwrap_or_default();
        if boot_vga.trim() == "1" {
            return format!("/dev/dri/{n}");
        }
    }
    nodes.first().map(|n| format!("/dev/dri/{n}")).unwrap_or_else(|| "/dev/dri/renderD128".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const FP_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn the_one_mac_format_reads_as_one_paired_mac() {
        let c = Config::parse(&format!(
            "host = \"MAC1.local:44120\"\nhost_fingerprint = \"{FP_A}\"\nscale = 1\ncapture_scale = 0\nrender_node = \"/dev/dri/renderD129\"\n"
        ))
        .unwrap();
        assert_eq!(c.host, "MAC1.local:44120");
        assert_eq!(c.host_fingerprint, FP_A);
        assert_eq!(c.macs.len(), 1);
        assert_eq!(c.macs[0].name, "MAC1");
        assert_eq!(c.render_node, "/dev/dri/renderD129");
    }

    #[test]
    fn the_corners_setting_round_trips() {
        // (A line from a version with the icon badge, "badge = false", is read past.)
        let mut c = Config::parse(&format!("host = \"MAC1.local:44120\"\nhost_fingerprint = \"{FP_A}\"\nbadge = false\n")).unwrap();
        assert!(!c.render().contains("badge"));
        assert_eq!(Config::parse(&c.render()).unwrap().square_corners, None, "not set unless chosen");
        c.square_corners = Some(false);
        assert_eq!(Config::parse(&c.render()).unwrap().square_corners, Some(false));
    }

    #[test]
    fn several_macs_round_trip_and_switch() {
        let mut c = Config::parse(&format!("host = \"MAC1.local:44120\"\nhost_fingerprint = \"{FP_A}\"\n")).unwrap();
        c.add_mac(PairedMac { name: "Air".into(), service: "Air".into(), host: "air.local:44120".into(), fingerprint: FP_B.into(), addresses: vec!["192.168.1.7".into(), "100.64.0.3".into(), "Air.local".into()] }, true);
        let back = Config::parse(&c.render()).unwrap();
        let air = back.macs.iter().find(|m| m.fingerprint == FP_B).unwrap();
        assert_eq!(air.addresses, ["192.168.1.7", "100.64.0.3", "Air.local"]);
        assert_eq!(places(air), ["air.local:44120", "192.168.1.7:44120", "100.64.0.3:44120", "Air.local:44120"]);
        assert_eq!(c.host_fingerprint, FP_B);
        let again = Config::parse(&c.render()).unwrap();
        assert_eq!(again.macs, c.macs);
        assert_eq!(again.host, "air.local:44120");
        assert_eq!(again.service, "Air");

        let mut again = again;
        assert!(again.activate(FP_A));
        assert_eq!(Config::parse(&again.render()).unwrap().host_fingerprint, FP_A);
        again.forget(FP_A);
        assert_eq!(again.host_fingerprint, FP_B);
        again.forget(FP_B);
        assert!(again.host.is_empty() && again.macs.is_empty());
    }
}
