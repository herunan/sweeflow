//! Launcher entries for the Mac's apps, so they show up in KDE's app menu / KRunner and the
//! taskbar can tell each Mac app apart (it matches windows to entries by StartupWMClass).

use std::{fs, path::PathBuf};

use anyhow::Result;

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
}

/// Keeps ~/.local/share/applications/sweeflow-*.desktop in sync with the Mac's installed apps.
pub fn sync(apps: &[(String, String, Vec<u8>)]) -> Result<()> {
    let apps_dir = data_dir().join("applications");
    let icon_dir = data_dir().join("sweeflow/icons");
    fs::create_dir_all(&apps_dir)?;
    fs::create_dir_all(&icon_dir)?;
    let exe = std::env::current_exe()?;
    let mut keep = std::collections::HashSet::new();
    for (bid, name, png) in apps {
        // Bundle ids are reverse-DNS, safe for file names; guard anyway.
        if bid.is_empty() || bid.contains('/') {
            continue;
        }
        let icon = icon_dir.join(format!("{bid}.png"));
        if !png.is_empty() && fs::read(&icon).ok().as_deref() != Some(png.as_slice()) {
            fs::write(&icon, png)?;
        }
        let file = format!("sweeflow-{bid}.desktop");
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName={name} (Mac)\nGenericName=Mac app\nComment=Runs on your Mac, shown here by sweeflow\n\
             Exec={} launch {bid}\nIcon={}\nStartupWMClass=sweeflow-{bid}\nCategories=Sweeflow;\nTerminal=false\n",
            exe.display(),
            icon.display()
        );
        let path = apps_dir.join(&file);
        if fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str()) {
            fs::write(&path, entry)?;
        }
        keep.insert(file);
    }
    // Apps uninstalled on the Mac.
    for e in fs::read_dir(&apps_dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("sweeflow-") && name.ends_with(".desktop") && !keep.contains(&name) {
            let _ = fs::remove_file(e.path());
        }
    }
    log::info!("launcher entries for {} Mac apps", keep.len());
    Ok(())
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("sweeflow.sock")
}

/// `sweeflow launch <bundle id>`: asks the running client to open the app on the Mac.
pub fn launch(bundle_id: &str) -> Result<()> {
    use std::io::Write;
    let mut s = std::os::unix::net::UnixStream::connect(socket_path())
        .map_err(|e| anyhow::anyhow!("sweeflow isn't running ({e}); start it with `systemctl --user start sweeflow`"))?;
    writeln!(s, "launch {bundle_id}")?;
    Ok(())
}
