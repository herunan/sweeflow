mod arrival;
mod blind;
mod clipboard;
mod config;
mod decode;
mod deskkeys;
mod desktop;
mod frames;
mod globalkeys;
mod hyprland;
mod keptkeys;
mod keys;
mod launchers;
mod lights;
mod linuxkeys;
mod look;
mod menus;
mod net;
mod pair;
mod pake;
mod palette;
mod panel;
mod panelwin;
mod placement;
mod prompt;
mod proto;
mod setup;
mod tray;
mod ui;
mod update;
mod updatewin;
mod wake;
mod wizard;
mod x11clip;
mod x11gl;
mod x11ui;

use anyhow::Result;

const USAGE: &str = "\
sweeflow — stream Mac app windows into your Linux desktop

USAGE:
    sweeflow                 stream from the linked Mac: its app windows open here
    sweeflow open            start Sweeflow if needed and show its controls (the app menu entry)
    sweeflow toggle-pause    pause streaming, or resume it when paused (Cmd+Opt+Y)
    sweeflow toggle-stream   stop streaming, or start it when stopped (Cmd+Opt+Z)
    sweeflow panel           Sweeflow's own window: its apps, settings and Macs (without the Plasma widget)
    sweeflow pair            link a Mac on this network (the Mac shows a code, you type it here)
    sweeflow pair --text     the same, in the terminal
    sweeflow update          install the latest release (install.sh's copy; packages say how)
    sweeflow update-window   the update window: checks, shows what's new, updates
    sweeflow setup USER@MAC  install/update the Mac host over SSH and pair with it (development)
    sweeflow launch BUNDLEID open a Mac app (what the Mac app launchers run)
    sweeflow quit BUNDLEID   quit a Mac app (the Mac app launchers' Quit action)
    sweeflow menu            the last focused Mac app's menu, to browse or search and choose an item
    sweeflow install         install this binary and start it with your desktop (systemd user service)
    sweeflow uninstall       undo `install` for this user: service, launchers, icons, widget, shortcuts
                             (keeps the pairing with the Mac; --purge removes it too)
    sweeflow status          show configuration and pairing info
";

/// The Mac serves one client at a time, so a second copy would keep kicking the first off.
/// Holds an exclusive lock for the life of the process.
fn single_instance() -> Result<std::fs::File> {
    match lock("sweeflow.lock")? {
        Some(f) => Ok(f),
        None => anyhow::bail!("sweeflow is already running (see `systemctl --user status sweeflow`)"),
    }
}

/// An exclusive lock (a file in the runtime folder) held until the returned file is dropped, or
/// None when another process holds it.
pub(crate) fn lock(name: &str) -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let dir = crate::config::runtime_dir();
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(name))?;
    // SAFETY: flock on a valid, owned descriptor.
    Ok((unsafe { libc_flock(f.as_raw_fd(), 2 | 4) } == 0).then_some(f))
}

unsafe extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, op: i32) -> i32;
}

/// The config once a Mac is paired. Nothing paired yet (a fresh install): open the "Link a Mac"
/// window once, and wait (rather than exit, which systemd would answer by starting us again).
fn wait_for_a_mac() -> Result<config::Config> {
    let mut asked = false;
    loop {
        let cfg = config::Config::load_or_default()?;
        if !cfg.host.is_empty() {
            return Ok(cfg);
        }
        if !asked {
            // The desktop's dialog about Sweeflow's keys first (globalkeys), on its own.
            globalkeys::wait_until_asked(std::time::Duration::from_secs(120));
            log::info!("no Mac paired yet: opening the pairing window");
            tray::open_wizard();
            asked = true;
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") | Some("version") => {
            println!("sweeflow {} ({})", update::VERSION, update::build());
            Ok(())
        }
        None | Some("run") => {
            // Started by hand (a terminal, KRunner) while installed as a service: start the
            // service instead, so there's one copy and Quit / restarts behave. (Not by
            // INVOCATION_ID: KDE runs launched apps as services of their own, which have one too.)
            let in_service = std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| c.contains("/sweeflow.service"));
            if !in_service && setup::service_installed() {
                eprintln!("sweeflow runs as a user service: starting it (systemctl --user start sweeflow)");
                let ok = std::process::Command::new("systemctl").args(["--user", "start", "sweeflow"]).status()?.success();
                anyhow::ensure!(ok, "couldn't start the sweeflow service");
                return Ok(());
            }
            let _lock = single_instance()?;
            log::info!("Sweeflow {} ({})", update::VERSION, update::build());
            if in_service {
                // A package's upgrade or removal, which can't reach a user's running copy.
                update::watch_binary();
                // Installs from before Sweeflow was in the app menu.
                if let Some(home) = std::env::var_os("HOME") {
                    let _ = setup::write_launcher(std::path::Path::new(&home));
                    setup::desktop_commands_now(std::path::Path::new(&home));
                }
            }
            hyprland::add_to_config();
            // Before a Mac is linked too: the tray icon is the way back to "Link a Mac".
            tray::start();
            // Sweeflow's keys from any window, the standard way where the desktop has it.
            globalkeys::start();
            // A newer release, looked for now and then (the panel and a notification say so).
            update::start();
            if x11ui::wanted() { x11ui::run(wait_for_a_mac()?) } else { ui::run(wait_for_a_mac()?) }
        }
        Some("pair") if args.get(1).is_some_and(|a| a == "--text") => pair::run_text(),
        Some("pair") => wizard::run(),
        Some("setup") => setup::setup(args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?),
        Some("install") => setup::install(),
        Some("update") => update::run(),
        Some("update-window") => updatewin::run(&args[1..]),
        Some("uninstall") if args.len() == 1 => setup::uninstall(false),
        Some("uninstall") if args.len() == 2 && args[1] == "--purge" => setup::uninstall(true),
        Some("open") => setup::open(),
        Some("toggle-pause") => setup::toggle("pause-toggle"),
        Some("toggle-stream") => setup::toggle("stream-toggle"),
        Some("launch") => launchers::launch(args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?),
        Some("quit") => launchers::quit(args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?),
        Some("menu") => palette::run(),
        Some("panel") => panelwin::run(),
        // Not in the usage: the Shortcuts window (Sweeflow's keys, keys kept for Linux).
        Some("shortcuts") | Some("linux-keys") => linuxkeys::run(),
        // Not in the usage: the keys kept for Linux over Mac windows (keptkeys.rs),
        // one per line, for checking a desktop from a terminal.
        Some("kept-keys") => {
            for k in keptkeys::all() {
                println!("{:<26} {:<24} {}", k.group, k.label, k.what);
            }
            Ok(())
        }
        // Not in the usage: what the client runs to show a Mac prompt (see prompt.rs).
        Some("prompt") => prompt::run(&args[1..]),
        // Not in the usage: draws made-up Mac windows through the X11 frontend (x11ui.rs).
        Some("x11-selftest") => x11ui::selftest(&args[1..]),
        Some("status") => setup::status(),
        _ => {
            eprint!("{USAGE}");
            Ok(())
        }
    }
}
