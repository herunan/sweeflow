//! Client configuration and identity, stored in ~/.config/sweeflow.

use std::{fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Config {
    /// host:port of the Mac.
    pub host: String,
    /// SHA-256 of the Mac's certificate (hex), learned at pairing.
    pub host_fingerprint: String,
    /// Mac points → Linux pixels. 1.0 on a scale-1 monitor.
    pub scale: f64,
    /// DRM render node of the GPU the compositor uses (decode there for zero-copy).
    pub render_node: String,
}

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("sweeflow")
}

impl Config {
    pub fn path() -> PathBuf {
        dir().join("config")
    }

    pub fn load() -> Result<Config> {
        let text = fs::read_to_string(Self::path())
            .with_context(|| format!("no config at {} — run `sweeflow setup user@mac` first", Self::path().display()))?;
        let mut c = Config { host: String::new(), host_fingerprint: String::new(), scale: 1.0, render_node: String::new() };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "host" => c.host = v,
                "host_fingerprint" => c.host_fingerprint = v,
                "scale" => c.scale = v.parse().context("scale")?,
                "render_node" => c.render_node = v,
                _ => {}
            }
        }
        if c.host.is_empty() || c.host_fingerprint.is_empty() {
            bail!("config is missing host or host_fingerprint — re-run `sweeflow setup`");
        }
        if c.render_node.is_empty() {
            c.render_node = "/dev/dri/renderD128".into();
        }
        Ok(c)
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(dir())?;
        fs::write(
            Self::path(),
            format!(
                "# sweeflow client configuration\nhost = \"{}\"\nhost_fingerprint = \"{}\"\nscale = {}\nrender_node = \"{}\"\n",
                self.host, self.host_fingerprint, self.scale, self.render_node
            ),
        )?;
        Ok(())
    }

    /// Returns (certificate, PKCS#8 key), creating them on first use.
    pub fn client_identity(&self) -> Result<(CertificateDer<'static>, Vec<u8>)> {
        client_identity()
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
