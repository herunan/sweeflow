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
        bind_stream_keys();
        keep_menu_focused();
        place_sweeflow_window();
        close_on_click_elsewhere();
        let Some(events) = events() else { return };
        for event in events {
            if event.starts_with("configreloaded") {
                hide_title_bars();
                bind_open_key();
                bind_stream_keys();
                keep_menu_focused();
                place_sweeflow_window();
                close_on_click_elsewhere();
            } else if event.starts_with("openlayer>>") || event.starts_with("closelayer>>") {
                // A bar came or went (Omarchy's shell starts after Sweeflow at login, or
                // restarts): the room it takes is known a moment later, and Sweeflow's window
                // is placed by it.
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    place_sweeflow_window();
                });
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
    // Where the click is, not which window has the focus: a click on the desktop or the bar
    // moves no focus. A click in the strip above a window placed under a top bar is the bar's
    // (its tray icon opens and closes the window itself).
    let lua = r#"if not sweeflow_click_closes2 then sweeflow_click_closes2 = true; hl.bind("mouse:272", function() local function ours(w) return w and w.class == "sweeflow" and (w.title == "Sweeflow" or string.find(w.title or "", "Global menu", 1, true)) end; local p = hl.get_cursor_pos(); for _, w in ipairs(hl.get_windows({ class = "sweeflow" })) do if ours(w) and w.at and w.size then local inside = p.x >= w.at.x and p.x < w.at.x + w.size.x and p.y >= w.at.y and p.y < w.at.y + w.size.y; local bar = w.at.y < 80 and p.y < w.at.y; if not inside and not bar then hl.dispatch(hl.dsp.window.close({ window = "address:" .. w.address })) end end end end, { non_consuming = true }) end"#;
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
    let Some(command) = own_command("open") else { return };
    // Once per Hyprland config (its Lua globals go with a reload, as the binding does): added
    // again by every start of the client, the key ran `sweeflow open` twice, which opens the
    // window and closes it again.
    // With a Mac-style key remapper (Toshy, Kinto), Cmd+Opt+O arrives as CTRL + ALT + O outside
    // the Mac windows: that key too.
    let keys: &[&str] = if crate::setup::mac_keys_remapped() { &["SUPER + ALT + O", "CTRL + ALT + O"] } else { &["SUPER + ALT + O"] };
    let binds: String =
        keys.iter().map(|k| format!(r#"hl.bind("{k}", hl.dsp.exec_cmd("{command}"), {{ description = "{OURS}{OPEN_NAME}" }}); "#)).collect();
    let lua = format!("if not sweeflow_open_key then sweeflow_open_key = true; {binds}end");
    match Command::new("hyprctl").args(["eval", &lua]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => log::info!("Hyprland: {} opens Sweeflow", keys.join(" or ")),
        Ok(o) => log::warn!("Hyprland didn't take the key for Sweeflow's window ({lua}): {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// `sweeflow <arg>` with the installed binary, the one the service runs: quoted for the shell
/// Hyprland runs it through, and the whole command for a Lua string.
fn own_command(arg: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let bin = std::path::Path::new(&home).join(".local/bin/sweeflow").display().to_string();
    Some(format!("'{}' {arg}", bin.replace('\'', r"'\''")).replace('\\', r"\\").replace('"', "\\\""))
}

/// SUPER + ALT + P (pause or resume) and SUPER + ALT + X (stop or start): `sweeflow
/// toggle-pause` / `toggle-stream` (setup::STREAM_KEYS), like the open key, but only on a key
/// nothing else has (Omarchy has SUPER + ALT + K for its tmux keys, say): the user's own binding
/// wins, and binding `sweeflow toggle-…` to other keys in their config is how they change them.
fn bind_stream_keys() {
    // Just after a config reload Hyprland may list nothing for a moment: asked again, and
    // nothing bound while it still lists nothing (a key the user has could be taken over).
    let mut bound = Vec::new();
    for _ in 0..10 {
        match Command::new("hyprctl").args(["binds", "-j"]).output() {
            Ok(o) => bound = parse_binds(&String::from_utf8_lossy(&o.stdout)),
            Err(e) => return log::debug!("hyprctl: {e}"),
        }
        if !bound.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if bound.is_empty() {
        return log::warn!("Hyprland lists no key bindings: Sweeflow's streaming keys not bound");
    }
    let mods: &[(&str, u32)] = if crate::setup::mac_keys_remapped() { &[("SUPER + ALT", SUPER_ALT), ("CTRL + ALT", CTRL_ALT)] } else { &[("SUPER + ALT", SUPER_ALT)] };
    let mut binds = String::new();
    let mut keys = Vec::new();
    for k in &crate::setup::STREAM_KEYS {
        let Some(command) = own_command(k.id) else { return };
        // The letter an earlier version bound (Cmd+Opt+P / X), while still ours: let go.
        for &(m, mask) in mods {
            if bound.iter().any(|b| b.0 == mask && b.1.eq_ignore_ascii_case(&k.old.to_string()) && b.2.starts_with(OURS)) {
                binds.push_str(&format!(r#"hl.unbind("{m} + {}"); "#, k.old));
            }
        }
        for &(m, mask) in mods {
            if let Some(by) = bound_by(&bound, mask, k.letter) {
                log::info!("Hyprland: {m} + {} is bound already ({by}): left alone, not set for \"{}\"", k.letter, k.name);
                continue;
            }
            binds.push_str(&format!(r#"hl.bind("{m} + {}", hl.dsp.exec_cmd("{command}"), {{ description = "{OURS}{}" }}); "#, k.letter, k.name));
            keys.push(format!("{m} + {} ({})", k.letter, k.name));
        }
    }
    if binds.is_empty() {
        return;
    }
    // Once per Hyprland config, as the open key. (Named for the letters: a running Hyprland that
    // has the earlier ones, P and X, takes these and lets those go.)
    let lua = format!("if not sweeflow_stream_keys_yz then sweeflow_stream_keys_yz = true; {binds}end");
    match Command::new("hyprctl").args(["eval", &lua]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {
            log::info!("Hyprland: {}", keys.join(", "));
            // The menu and panels show them.
            crate::keys::reread();
        }
        Ok(o) => log::warn!("Hyprland didn't take Sweeflow's streaming keys ({lua}): {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// Hyprland's modifier masks (`hyprctl binds -j`'s modmask): SUPER (64) + ALT (8), CTRL (4) +
/// ALT.
const SUPER_ALT: u32 = 64 | 8;
const CTRL_ALT: u32 = 4 | 8;
/// How Sweeflow's own bindings' descriptions start.
const OURS: &str = "Sweeflow: ";
const OPEN_NAME: &str = "Open Sweeflow";

/// The keys Hyprland has now for opening Sweeflow, pausing and stopping (as hyprland's modmask
/// and key): Sweeflow's own bindings (by their descriptions), or one in the user's config that
/// runs `sweeflow open` / `toggle-pause` / `toggle-stream`. (For showing them.)
pub fn our_keys() -> [Option<(u32, String)>; 3] {
    let Ok(o) = Command::new("hyprctl").args(["binds", "-j"]).output() else { return Default::default() };
    find_ours(&String::from_utf8_lossy(&o.stdout))
}

fn find_ours(json: &str) -> [Option<(u32, String)>; 3] {
    let Ok(serde_json::Value::Array(all)) = serde_json::from_str(json) else { return Default::default() };
    let wanted = [(OPEN_NAME, "open"), (crate::setup::STREAM_KEYS[0].name, crate::setup::STREAM_KEYS[0].id), (crate::setup::STREAM_KEYS[1].name, crate::setup::STREAM_KEYS[1].id)];
    wanted.map(|(name, arg)| {
        let described = format!("{OURS}{name}");
        let runs = |b: &serde_json::Value| {
            let a = b["arg"].as_str().unwrap_or("");
            b["dispatcher"].as_str().is_some_and(|d| d.starts_with("exec")) && a.contains("sweeflow") && a.split_whitespace().last() == Some(arg)
        };
        // A key that isn't a key (a mouse button, a submap's) doesn't count.
        let mut ours = all.iter().filter(|b| b["submap"].as_str().unwrap_or("").is_empty() && !b["mouse"].as_bool().unwrap_or(false));
        ours.find(|b| b["description"].as_str() == Some(described.as_str()) || runs(b))
            .and_then(|b| Some((b["modmask"].as_u64()? as u32, b["key"].as_str()?.to_string())))
    })
}

/// The bindings `hyprctl binds -j` lists, as (modmask, key, description), outside submaps.
fn parse_binds(json: &str) -> Vec<(u32, String, String)> {
    let Ok(serde_json::Value::Array(all)) = serde_json::from_str(json) else { return Vec::new() };
    all.iter()
        .filter(|b| b["submap"].as_str().unwrap_or("").is_empty() && !b["mouse"].as_bool().unwrap_or(false))
        .filter_map(|b| {
            let mask = b["modmask"].as_u64()? as u32;
            Some((mask, b["key"].as_str()?.to_string(), b["description"].as_str().unwrap_or("").to_string()))
        })
        .collect()
}

/// What else has this key (its description, or "another binding"); None when it's free or
/// only Sweeflow's own.
fn bound_by(bound: &[(u32, String, String)], mask: u32, letter: char) -> Option<String> {
    bound
        .iter()
        .find(|b| b.0 == mask && b.1.eq_ignore_ascii_case(&letter.to_string()) && !b.2.starts_with(OURS))
        .map(|b| if b.2.is_empty() { "another binding".to_string() } else { b.2.clone() })
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

/// Floats this process's window of that class (and title, if several share it): a Mac window
/// its app won't let be resized, which a tile would cut off or leave empty around.
pub fn float_window(class: String, title: String) {
    std::thread::spawn(move || {
        let Ok(out) = Command::new("hyprctl").args(["-j", "clients"]).output() else { return };
        let Ok(serde_json::Value::Array(clients)) = serde_json::from_slice(&out.stdout) else { return };
        let pid = u64::from(std::process::id());
        let ours: Vec<_> = clients.iter().filter(|c| c["pid"].as_u64() == Some(pid) && c["class"].as_str() == Some(class.as_str())).collect();
        let Some(c) = ours.iter().find(|c| c["title"].as_str() == Some(title.as_str())).or(ours.first()) else { return };
        if c["floating"].as_bool() == Some(true) {
            return;
        }
        let Some(address) = c["address"].as_str() else { return };
        let lua = format!(r#"hl.dsp.window.float({{ action = "enable", window = "address:{address}" }})"#);
        let done = Command::new("hyprctl").args(["dispatch", &lua]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok");
        if !done {
            let _ = Command::new("hyprctl").args(["dispatch", "setfloating", &format!("address:{address}")]).output();
        }
    });
}

/// Moves this process's floating window of that class (and title) to the same spot of its
/// monitor's free area as on the Mac (fractions: 0 left/top, 1 right/bottom).
pub fn place_window(class: &str, title: &str, spot: (f32, f32)) {
    let json = |args: &[&str]| -> Option<serde_json::Value> { serde_json::from_slice(&Command::new("hyprctl").args(args).output().ok()?.stdout).ok() };
    let Some(serde_json::Value::Array(clients)) = json(&["-j", "clients"]) else { return };
    let pid = u64::from(std::process::id());
    let ours: Vec<_> = clients.iter().filter(|c| c["pid"].as_u64() == Some(pid) && c["class"].as_str() == Some(class)).collect();
    let Some(c) = ours.iter().find(|c| c["title"].as_str() == Some(title)).or(ours.first()) else { return };
    let Some(serde_json::Value::Array(monitors)) = json(&["-j", "monitors"]) else { return };
    let Some(m) = monitors.iter().find(|m| m["id"] == c["monitor"]) else { return };
    let n = |v: &serde_json::Value| v.as_f64().unwrap_or(0.0);
    let scale = n(&m["scale"]).max(0.1);
    let (left, top, right, bottom) = (n(&m["reserved"][0]), n(&m["reserved"][1]), n(&m["reserved"][2]), n(&m["reserved"][3]));
    let (ax, ay) = (n(&m["x"]) + left, n(&m["y"]) + top);
    let (aw, ah) = (n(&m["width"]) / scale - left - right, n(&m["height"]) / scale - top - bottom);
    let (w, h) = (n(&c["size"][0]), n(&c["size"][1]));
    let x = (ax + spot.0 as f64 * (aw - w).max(0.0)).round();
    let y = (ay + spot.1 as f64 * (ah - h).max(0.0)).round();
    let Some(address) = c["address"].as_str() else { return };
    let lua = format!(r#"hl.dsp.window.move({{ x = {x}, y = {y}, window = "address:{address}" }})"#);
    let _ = Command::new("hyprctl").args(["dispatch", &lua]).output();
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

    #[test]
    fn a_key_something_else_has_is_left_alone() {
        // As Omarchy lists them: SUPER + ALT + K is its tmux keys; a submap's and our own don't count.
        let json = r#"[
            {"modmask": 72, "key": "K", "submap": "", "mouse": false, "description": "Tmux keybindings"},
            {"modmask": 64, "key": "P", "submap": "", "mouse": false, "description": "Pseudo window"},
            {"modmask": 72, "key": "p", "submap": "resize", "mouse": false, "description": "In a submap"},
            {"modmask": 12, "key": "P", "submap": "", "mouse": false, "description": "Sweeflow: Pause or resume streaming"},
            {"modmask": 12, "key": "k", "submap": "", "mouse": false, "description": ""},
            {"modmask": 72, "key": "mouse:272", "submap": "", "mouse": true, "description": ""}
        ]"#;
        let bound = super::parse_binds(json);
        assert_eq!(bound.len(), 4);
        assert_eq!(super::bound_by(&bound, super::SUPER_ALT, 'K').as_deref(), Some("Tmux keybindings"));
        assert_eq!(super::bound_by(&bound, super::SUPER_ALT, 'P'), None);
        assert_eq!(super::bound_by(&bound, super::CTRL_ALT, 'P'), None);
        assert_eq!(super::bound_by(&bound, super::CTRL_ALT, 'K').as_deref(), Some("another binding"));
        assert!(super::parse_binds("not json").is_empty());
    }
}
