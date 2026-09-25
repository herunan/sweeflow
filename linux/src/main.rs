mod clipboard;
mod config;
mod decode;
mod launchers;
mod lights;
mod menus;
mod net;
mod positions;
mod proto;
mod setup;
mod ui;
mod wallpaper;

use anyhow::Result;

const USAGE: &str = "\
sweeflow — stream Mac app windows into your Linux desktop

USAGE:
    sweeflow                 connect to the paired Mac and show its windows
    sweeflow setup USER@MAC  install/update the Mac host over SSH and pair with it
    sweeflow launch BUNDLEID open a Mac app (what the Mac app launchers run)
    sweeflow install         install this binary and start it with your desktop (systemd user service)
    sweeflow status          show configuration and pairing info
";

/// The Mac serves one client at a time, so a second copy would keep kicking the first off.
/// Holds an exclusive lock for the life of the process.
fn single_instance() -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("sweeflow.lock"))?;
    // SAFETY: flock on a valid, owned descriptor.
    if unsafe { libc_flock(f.as_raw_fd(), 2 | 4) } != 0 {
        anyhow::bail!("sweeflow is already running (see `systemctl --user status sweeflow`)");
    }
    Ok(f)
}

unsafe extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, op: i32) -> i32;
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn")).init();
    ffmpeg_next::init()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("run") => {
            let _lock = single_instance()?;
            ui::run(config::Config::load()?)
        }
        Some("setup") => setup::setup(args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?),
        Some("install") => setup::install(),
        Some("launch") => launchers::launch(args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?),
        Some("status") => setup::status(),
        _ => {
            eprint!("{USAGE}");
            Ok(())
        }
    }
}
