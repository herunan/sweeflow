//! Hyprland (and so Omarchy), where the desktop gives a window less say than GNOME or Plasma do:
//! Sweeflow asks Hyprland itself, through `hyprctl` and its event socket.

use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    process::Command,
};

/// A Hyprland session.
pub fn running() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// Hyprland's event socket, when this is a Hyprland session: a line per event.
fn events() -> Option<impl Iterator<Item = String>> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let socket = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("hypr").join(signature).join(".socket2.sock");
    match UnixStream::connect(&socket) {
        Ok(events) => Some(BufReader::new(events).lines().map_while(Result::ok)),
        Err(e) => {
            log::info!("Hyprland's event socket ({}): {e}", socket.display());
            None
        }
    }
}

/// What Sweeflow adds to the running Hyprland: no title bars on its windows. Nothing is written
/// to the user's config, so a config reload drops it, and it's added again on every reload. (The
/// Global menu's key is caught by the Mac windows themselves, ui.rs: a Hyprland binding never
/// fires while one has focus, as they pass every shortcut through to the Mac.)
pub fn add_to_config() {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    std::thread::spawn(|| {
        hide_title_bars();
        bind_open_key();
        keep_menu_focused();
        place_sweeflow_window();
        close_on_click_elsewhere();
        let Some(events) = events() else { return };
        for event in events {
            if event.starts_with("configreloaded") {
                hide_title_bars();
                bind_open_key();
                keep_menu_focused();
                place_sweeflow_window();
                close_on_click_elsewhere();
            }
        }
    });
}

/// The Global menu keeps the keyboard while it is open. Hyprland's focus follows the pointer, so
/// a mouse resting on (or brushing) another window took the focus, and the menu, which closes
/// when it loses the focus, was gone before a letter was typed. A window rule for that one
/// window (by its title; Sweeflow's other windows have the same class).
fn keep_menu_focused() {
    let rule = r#"hl.window_rule({ name = "sweeflow-menu-keeps-focus", enabled = true, match = { class = "sweeflow", title = "(.* - )?Global menu" }, stay_focused = true })"#;
    match Command::new("hyprctl").args(["eval", rule]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {}
        Ok(o) => log::warn!("Hyprland didn't take the rule that keeps the Global menu focused: {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// Sweeflow's own window opens where a tray popup would: in the corner by the bar (top right
/// under a top bar, as on Omarchy; bottom right above a bottom one), not in the middle of the
/// screen. A window rule for that one window, by its title. Where the bar is comes from the
/// room it reserves on the focused monitor.
fn place_sweeflow_window() {
    let reserved = Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|monitors| {
            let all = monitors.as_array()?;
            let m = all.iter().find(|m| m["focused"].as_bool() == Some(true)).or(all.first())?;
            Some((m["reserved"][1].as_i64().unwrap_or(0), m["reserved"][3].as_i64().unwrap_or(0)))
        })
        .unwrap_or((0, 0));
    let rule = place_rule_lua(reserved.0, reserved.1);
    match Command::new("hyprctl").args(["eval", &rule]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {}
        Ok(o) => log::warn!("Hyprland didn't take the rule that places Sweeflow's window: {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// The rule's Lua, for a bar taking `top` or `bottom` points of the screen (a gap of 8 from it
/// and from the screen's edge).
fn place_rule_lua(top: i64, bottom: i64) -> String {
    let y = if top == 0 && bottom > 0 { format!("(monitor_h-window_h-{})", bottom + 8) } else { (top + 8).to_string() };
    format!(
        r#"hl.window_rule({{ name = "sweeflow-window-place", enabled = true, match = {{ class = "sweeflow", title = "Sweeflow" }}, float = true, move = {{ "(monitor_w-window_w-8)", "{y}" }} }})"#
    )
}

/// A click anywhere else closes Sweeflow's window and the Global menu, as it closes a tray
/// popup. Hyprland's focus follows the pointer, so "lost the focus" can't mean that there (it
/// happens on a mere hover): instead Hyprland itself looks at every left click (a binding that
/// lets the click through) and closes the two when the click is not on one of them. Once per
/// Hyprland config, like the key.
fn close_on_click_elsewhere() {
    let lua = r#"if not sweeflow_click_closes then sweeflow_click_closes = true; hl.bind("mouse:272", function() local function ours(w) return w and w.class == "sweeflow" and (w.title == "Sweeflow" or string.find(w.title or "", "Global menu", 1, true)) end; if ours(hl.get_active_window()) then return end; for _, w in ipairs(hl.get_windows({ class = "sweeflow" })) do if ours(w) then hl.dispatch(hl.dsp.window.close({ window = "address:" .. w.address })) end end end, { non_consuming = true }) end"#;
    match Command::new("hyprctl").args(["eval", lua]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {}
        Ok(o) => log::warn!("Hyprland didn't take the click-elsewhere binding: {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// SUPER + ALT + O opens Sweeflow (its window: `sweeflow open`) from anywhere. The key is free
/// on Omarchy. Runs the installed binary, the one the service runs. A config reload drops the
/// binding, so it is added again after each.
fn bind_open_key() {
    let Some(home) = std::env::var_os("HOME") else { return };
    let bin = std::path::Path::new(&home).join(".local/bin/sweeflow").display().to_string();
    // Quoted for the shell Hyprland runs it through, and the whole command for Lua.
    let command = format!("'{}' open", bin.replace('\'', r"'\''")).replace('\\', r"\\").replace('"', "\\\"");
    // Once per Hyprland config (its Lua globals go with a reload, as the binding does): added
    // again by every start of the client, the key ran `sweeflow open` twice, which opens the
    // window and closes it again.
    let lua = format!(r#"if not sweeflow_open_key then sweeflow_open_key = true; hl.bind("SUPER + ALT + O", hl.dsp.exec_cmd("{command}")) end"#);
    match Command::new("hyprctl").args(["eval", &lua]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => log::info!("Hyprland: SUPER + ALT + O opens Sweeflow"),
        Ok(o) => log::warn!("Hyprland didn't take the key for Sweeflow's window ({lua}): {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// No title bars on Sweeflow's windows. Omarchy draws one on every window of a floating
/// workspace; a Mac window already has the Mac's own, and Sweeflow's dialogs carry their heading
/// inside. Done with a window rule.
fn hide_title_bars() {
    // The rule belongs to Omarchy's title bar plugin. Without the plugin (plain Hyprland, or
    // before anything has floated) Hyprland refuses the rule; loading the plugin reloads the
    // config, which brings this round again.
    let rule = r#"hl.window_rule({ name = "sweeflow-no-title-bar", enabled = true, match = { class = "sweeflow(-.+)?" }, ["hyprbars:no_bar"] = true })"#;
    match Command::new("hyprctl").args(["eval", rule]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => log::info!("Hyprland: no title bars on Sweeflow's windows"),
        Ok(o) => log::debug!("Hyprland: title bar rule not taken: {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// Tells the window side which of this process's windows Hyprland has tiled: now, and whenever
/// a window opens, changes between floating and tiled, moves or gets a new title. Hyprland
/// doesn't tell the windows themselves (other desktops do, through the window's own state).
pub fn watch_tiling() {
    if !running() {
        return;
    }
    let (changed, changes) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = changed.send(());
        let Some(events) = events() else { return };
        for event in events {
            if ["openwindow>>", "changefloatingmode>>", "movewindow", "windowtitle"].iter().any(|e| event.starts_with(e)) {
                let _ = changed.send(());
            }
        }
    });
    std::thread::spawn(move || {
        let pid = u64::from(std::process::id());
        while changes.recv().is_ok() {
            // A burst (a whole workspace switching layout) settles first: one look for it.
            while changes.recv_timeout(std::time::Duration::from_millis(80)).is_ok() {}
            let Ok(out) = Command::new("hyprctl").args(["-j", "clients"]).output() else { continue };
            let Ok(serde_json::Value::Array(clients)) = serde_json::from_slice(&out.stdout) else { continue };
            let ours = clients
                .iter()
                .filter(|c| c["pid"].as_u64() == Some(pid))
                .map(|c| (c["class"].as_str().unwrap_or_default().to_string(), c["title"].as_str().unwrap_or_default().to_string(), c["floating"].as_bool().unwrap_or(true)))
                .collect();
            crate::net::tell_ui(crate::net::UiEvent::Tiling(ours));
        }
    });
}

/// Hyprland frames every window as a plain rectangle (no rounding set, as on Omarchy): a Mac
/// window's rounded corners would leave gaps inside that frame. Looked at once.
pub fn square_windows() -> bool {
    static SQUARE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
            && Command::new("hyprctl")
                .args(["-j", "getoption", "decoration:rounding"])
                .output()
                .ok()
                .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
                .is_some_and(|v| v["int"].as_i64() == Some(0))
    });
    *SQUARE
}

/// Maximises the focused window, or puts it back, through Hyprland itself. False when this
/// isn't Hyprland (the window then asks its desktop the usual way). For a Mac window's green
/// button: the click has just focused that window.
pub fn toggle_maximise() -> bool {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return false;
    }
    std::thread::spawn(|| {
        // Hyprland 0.5x takes Lua; earlier ones the plain dispatcher.
        let done = Command::new("hyprctl")
            .args(["dispatch", r#"hl.dsp.window.fullscreen({ mode = "maximized", action = "toggle" })"#])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok");
        if !done {
            let _ = Command::new("hyprctl").args(["dispatch", "fullscreen", "1"]).output();
        }
    });
    true
}

/// Keeps this process's window at its own size: Hyprland maximises any window it is told to
/// (the button in Omarchy's title bars, a key), whatever size the window says it has. Hyprland
/// says so on its event socket the moment it happens, and the window is taken straight back out.
pub fn keep_window_size() {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    std::thread::spawn(|| {
        let Some(events) = events() else { return };
        for event in events {
            if event.starts_with("fullscreen>>1") {
                unmaximise();
            }
        }
    });
}

/// Takes this process's window out of Hyprland's maximised or full screen state. Hyprland keeps
/// it there whatever the window asks (Omarchy tells it to ignore windows' own requests), so its
/// own command does it.
fn unmaximise() {
    let Ok(out) = Command::new("hyprctl").args(["-j", "clients"]).output() else { return };
    let Ok(serde_json::Value::Array(clients)) = serde_json::from_slice(&out.stdout) else { return };
    let pid = u64::from(std::process::id());
    for c in clients.iter().filter(|c| c["pid"].as_u64() == Some(pid) && c["fullscreen"].as_u64().is_some_and(|f| f != 0)) {
        let Some(address) = c["address"].as_str() else { continue };
        let mode = if c["fullscreen"].as_u64() == Some(2) { "fullscreen" } else { "maximized" };
        // Hyprland 0.5x takes Lua; earlier ones the plain dispatcher, which acts on the focused
        // window (the one just maximised).
        let lua = format!(r#"hl.dsp.window.fullscreen({{ mode = "{mode}", action = "unset", window = "address:{address}" }})"#);
        let done = Command::new("hyprctl")
            .args(["dispatch", &lua])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok");
        if !done {
            let _ = Command::new("hyprctl").args(["dispatch", "fullscreenstate", "0 0"]).output();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sweeflow_window_goes_by_the_bar() {
        assert!(super::place_rule_lua(26, 0).contains(r#"move = { "(monitor_w-window_w-8)", "34" }"#));
        assert!(super::place_rule_lua(0, 30).contains(r#""(monitor_h-window_h-38)""#));
        assert!(super::place_rule_lua(0, 0).contains(r#""8" }"#));
    }
}
