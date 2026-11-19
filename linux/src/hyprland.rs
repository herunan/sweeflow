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

/// Run outside the Hyprland session (`sweeflow uninstall` over SSH, from a TTY): the session that
/// is running, found by its socket, so its bindings can still be taken away. Only when there is
/// exactly one. Call before any thread starts (it sets the environment).
pub fn find_session() {
    if running() {
        return;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from) else { return };
    let Ok(dirs) = std::fs::read_dir(runtime.join("hypr")) else { return };
    let live: Vec<_> = dirs.flatten().filter(|d| d.path().join(".socket.sock").exists()).map(|d| d.file_name()).collect();
    if let [one] = live.as_slice() {
        // SAFETY: only from `sweeflow uninstall`'s start, before it has started any thread.
        unsafe { std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", one) };
    }
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
    if !running() {
        return;
    }
    std::thread::spawn(|| {
        hide_title_bars();
        bind_own_keys();
        keep_menu_focused();
        place_sweeflow_window();
        close_on_click_elsewhere();
        let Some(events) = events() else { return };
        for event in events {
            if event.starts_with("configreloaded") {
                hide_title_bars();
                bind_own_keys();
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
/// window (by its title; Sweeflow's other windows have the same class). Sweeflow's own window
/// too: Esc (which closes it) went to whatever window the pointer had moved onto. A click
/// outside either still closes it.
fn keep_menu_focused() {
    let rule = r#"hl.window_rule({ name = "sweeflow-menu-keeps-focus", enabled = true, match = { class = "sweeflow", title = "^((.* - )?Global Menu|Sweeflow)$" }, stay_focused = true })"#;
    match Command::new("hyprctl").args(["eval", rule]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {}
        Ok(o) => log::warn!("Hyprland didn't take the rule that keeps the Global Menu focused: {}", String::from_utf8_lossy(&o.stdout).trim()),
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
    let lua = r#"if not sweeflow_click_closes3 then sweeflow_click_closes3 = true; hl.bind("mouse:272", function() local function ours(w) return w and w.class == "sweeflow" and (w.title == "Sweeflow" or string.find(w.title or "", "Global Menu", 1, true)) end; local p = hl.get_cursor_pos(); for _, w in ipairs(hl.get_windows({ class = "sweeflow" })) do if ours(w) and w.at and w.size then local inside = p.x >= w.at.x and p.x < w.at.x + w.size.x and p.y >= w.at.y and p.y < w.at.y + w.size.y; local bar = w.at.y < 80 and p.y < w.at.y; if not inside and not bar then hl.dispatch(hl.dsp.window.close({ window = "address:" .. w.address })) end end end end, { non_consuming = true }) end"#;
    match Command::new("hyprctl").args(["eval", lua]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {}
        Ok(o) => log::warn!("Hyprland didn't take the click-elsewhere binding: {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// `sweeflow <arg>` with the installed binary, the one the service runs: quoted for the shell
/// Hyprland runs it through, and the whole command for a Lua string.
fn own_command(arg: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let bin = crate::setup::installed_binary(std::path::Path::new(&home)).display().to_string();
    Some(format!("'{}' {arg}", bin.replace('\'', r"'\''")).replace('\\', r"\\").replace('"', "\\\""))
}

/// Sweeflow's keys from anywhere (as set in its Shortcuts window, keys.rs: Super+Alt+O opens it,
/// Y pauses or resumes, Z stops or starts), bound to the installed binary's `sweeflow open` /
/// `toggle-pause` / `toggle-stream`. Each time: the ones bound before let go first, so a key
/// changed in the Shortcuts window replaces the old one (`rebind_own_keys`); a key the user has
/// bound to something else is left to them. With a Mac-style key remapper (Toshy, Kinto), ⌘
/// arrives as CTRL outside the Mac windows: that key too. A config reload drops them all, so they
/// are bound again after each.
fn bind_own_keys() {
    // Just after a config reload Hyprland may list nothing for a moment: asked again, and
    // nothing bound while it still lists nothing (a key the user has could be taken over).
    let mut listed = String::new();
    let mut bound = Vec::new();
    for _ in 0..10 {
        match Command::new("hyprctl").args(["binds", "-j"]).output() {
            Ok(o) => {
                listed = String::from_utf8_lossy(&o.stdout).to_string();
                bound = parse_binds(&listed);
            }
            Err(e) => return log::debug!("hyprctl: {e}"),
        }
        if !bound.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if bound.is_empty() {
        return log::warn!("Hyprland lists no key bindings: Sweeflow's keys not bound");
    }
    let (mut lua, _) = unbind_lua(&listed);
    let remapped = crate::setup::mac_keys_remapped();
    let mut keys = Vec::new();
    for (name, arg, what) in [("open", "open", OPEN_NAME), ("pause", "toggle-pause", "Pause or resume streaming"), ("stop", "toggle-stream", "Stop or start streaming")] {
        let Some(k) = crate::keys::own_key(name) else { continue };
        let Some(command) = own_command(arg) else { return };
        let mut variants = vec![k.clone()];
        if remapped && k.logo && !k.ctrl {
            variants.push(crate::deskkeys::UserKey { logo: false, ctrl: true, ..k.clone() });
        }
        for v in variants {
            let mask = (v.shift as u32) | ((v.ctrl as u32) << 2) | ((v.alt as u32) << 3) | ((v.logo as u32) << 6);
            let key = hypr_key(&v.key);
            if let Some(by) = taken_by(&bound, mask, &key) {
                log::info!("Hyprland: {} is bound already ({by}): left alone, not set for \"{what}\"", v.label());
                continue;
            }
            let Some(mods) = mods_of(mask) else { continue };
            lua.push_str(&format!(r#"hl.bind("{mods} + {}", hl.dsp.exec_cmd("{command}"), {{ description = "{OURS}{what}" }}); "#, key.to_uppercase()));
            keys.push(format!("{} ({what})", v.label()));
        }
    }
    match Command::new("hyprctl").args(["eval", &lua]).output() {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => log::info!("Hyprland: {}", keys.join(", ")),
        Ok(o) => log::warn!("Hyprland didn't take Sweeflow's keys ({lua}): {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => log::debug!("hyprctl: {e}"),
    }
}

/// What else has a key in Hyprland's bindings (its description, or "another binding"); None
/// when nothing or only Sweeflow does.
fn taken_by(bound: &[(u32, String, String)], mask: u32, key: &str) -> Option<String> {
    bound
        .iter()
        .find(|b| b.0 == mask && b.1.eq_ignore_ascii_case(key) && !b.2.starts_with(OURS))
        .map(|b| if b.2.is_empty() { "another binding".to_string() } else { b.2.clone() })
}

/// What a key is bound to already in Hyprland, other than Sweeflow ("Google Photos", or "another
/// binding" without a description): Sweeflow leaves such a key alone, so its Shortcuts window
/// says so instead of showing it as set.
pub fn bound_elsewhere(k: &crate::deskkeys::UserKey) -> Option<String> {
    if !running() {
        return None;
    }
    let o = Command::new("hyprctl").args(["binds", "-j"]).output().ok()?;
    let bound = parse_binds(&String::from_utf8_lossy(&o.stdout));
    let mask = (k.shift as u32) | ((k.ctrl as u32) << 2) | ((k.alt as u32) << 3) | ((k.logo as u32) << 6);
    taken_by(&bound, mask, &hypr_key(&k.key))
}

/// Sweeflow's keys were changed (its Shortcuts window): bound again.
pub fn rebind_own_keys() {
    if running() {
        std::thread::spawn(bind_own_keys);
    }
}

/// Hyprland's modifier masks (`hyprctl binds -j`'s modmask): SUPER (64) + ALT (8), CTRL (4) +
/// ALT.
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

/// `sweeflow uninstall`: Sweeflow's key bindings let go in the running Hyprland, and its marks
/// that they were added, so a new install adds them again. Nothing of Sweeflow's is in the config
/// files: its window rules and the click that closes its window go with the next config reload
/// or login (they only ever touch Sweeflow's windows). How many keys were let go.
pub fn unbind_ours() -> anyhow::Result<usize> {
    let o = Command::new("hyprctl").args(["binds", "-j"]).output()?;
    let (unbinds, n) = unbind_lua(&String::from_utf8_lossy(&o.stdout));
    let lua = format!("sweeflow_open_key = nil; sweeflow_stream_keys_yz = nil; {unbinds}");
    let o = Command::new("hyprctl").args(["eval", &lua]).output()?;
    let said = String::from_utf8_lossy(&o.stdout).trim().to_string();
    anyhow::ensure!(said == "ok", "Hyprland said {said}");
    Ok(n)
}

/// The Lua that lets go of Sweeflow's bindings (by their descriptions) among those `hyprctl binds
/// -j` lists, and how many keys. Unbinding goes by the key, so a key that has a binding of the
/// user's too (the open key is bound whether or not it's free) is left alone: theirs would go.
fn unbind_lua(json: &str) -> (String, usize) {
    let bound = parse_binds(json);
    let mut keys: Vec<String> = Vec::new();
    for (mask, key, description) in &bound {
        if !description.starts_with(OURS) || bound.iter().any(|b| b.0 == *mask && b.1.eq_ignore_ascii_case(key) && !b.2.starts_with(OURS)) {
            continue;
        }
        let Some(mods) = mods_of(*mask) else { continue };
        let key = format!("{mods} + {}", key.to_uppercase());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    (keys.iter().map(|k| format!(r#"hl.unbind("{k}"); "#)).collect(), keys.len())
}

/// A modmask as Hyprland's key names ("SUPER + ALT"); None for one with modifiers Sweeflow never
/// binds.
fn mods_of(mask: u32) -> Option<String> {
    const NAMES: [(u32, &str); 4] = [(64, "SUPER"), (4, "CTRL"), (8, "ALT"), (1, "SHIFT")];
    let known = NAMES.iter().fold(0, |m, n| m | n.0);
    if mask == 0 || mask & !known != 0 {
        return None;
    }
    Some(NAMES.iter().filter(|n| mask & n.0 != 0).map(|n| n.1).collect::<Vec<_>>().join(" + "))
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
    float_window_sized(class, title, None);
}

/// As float_window, then at this size (points), centred: Hyprland floats a window at the size
/// and place its tile had (a video player came back cut off, half off the screen's edge).
pub fn float_window_sized(class: String, title: String, size: Option<(u32, u32)>) {
    std::thread::spawn(move || {
        // (A window just made may not be mapped yet: looked for again for up to a second.)
        let pid = u64::from(std::process::id());
        let mut clients = Vec::new();
        for _ in 0..10 {
            let Ok(out) = Command::new("hyprctl").args(["-j", "clients"]).output() else { return };
            if let Ok(serde_json::Value::Array(list)) = serde_json::from_slice(&out.stdout) {
                clients = list;
            }
            if clients.iter().any(|c| c["pid"].as_u64() == Some(pid) && c["class"].as_str() == Some(class.as_str())) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
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
        if let Some((w, h)) = size {
            let window = format!("address:{address}");
            for lua in [
                format!(r#"hl.dsp.window.resize({{ x = {w}, y = {h}, window = "{window}" }})"#),
                format!(r#"hl.dsp.window.center({{ window = "{window}" }})"#),
            ] {
                let _ = Command::new("hyprctl").args(["dispatch", &lua]).output();
            }
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

/// Runs a dispatcher: Hyprland 0.5x takes Lua (`lua`), earlier ones the plain form (`legacy`).
fn dispatch(lua: &str, legacy: &[&str]) {
    let done = Command::new("hyprctl").args(["dispatch", lua]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok");
    if !done {
        let _ = Command::new("hyprctl").arg("dispatch").args(legacy).output();
    }
}

/// This process's window of that class (and title, if several share it), as `hyprctl clients`
/// lists it.
fn our_client(class: &str, title: &str) -> Option<serde_json::Value> {
    our_client_where(class, |c| c["title"].as_str() == Some(title))
}

/// As `our_client`, the first that `best` picks (else the first of that class).
fn our_client_where(class: &str, best: impl Fn(&serde_json::Value) -> bool) -> Option<serde_json::Value> {
    let out = Command::new("hyprctl").args(["-j", "clients"]).output().ok()?;
    let serde_json::Value::Array(clients) = serde_json::from_slice(&out.stdout).ok()? else { return None };
    let pid = u64::from(std::process::id());
    let ours: Vec<_> = clients.into_iter().filter(|c| c["pid"].as_u64() == Some(pid) && c["class"].as_str() == Some(class)).collect();
    let i = ours.iter().position(best).unwrap_or(0);
    ours.into_iter().nth(i)
}

/// Where minimised windows wait: Hyprland's usual stand-in for minimising, and where Omarchy's
/// own minimise puts them (its dock brings them back from there).
const MINIMIZED: &str = "special:minimized";

/// Minimises this process's window of that class (and title): Hyprland has no minimise of its
/// own and ignores a window asking to be, so the Mac's yellow button did nothing. As Omarchy's
/// own minimise does: to the hidden "minimized" workspace, tagged with where it came from (its
/// dock lists it and brings it back there). On any workspace, tiled ones too, where Omarchy's
/// own Super+M does nothing: launching the app brings it back (`unminimise`). False when this
/// isn't Hyprland.
pub fn minimise(class: String, title: String) -> bool {
    if !running() {
        return false;
    }
    std::thread::spawn(move || {
        let Some(c) = our_client(&class, &title) else { return };
        let Some(address) = c["address"].as_str() else { return };
        let window = format!("address:{address}");
        let from = c["workspace"]["id"].as_i64().filter(|&w| w >= 1);
        if let Some(ws) = from {
            let tag = format!("+omarchy-minimized-from-{ws}");
            dispatch(&format!(r#"hl.dsp.window.tag({{ tag = "{tag}", window = "{window}" }})"#), &["tagwindow", &tag, &window]);
        }
        dispatch(
            &format!(r#"hl.dsp.window.move({{ workspace = "{MINIMIZED}", follow = false, window = "{window}" }})"#),
            &["movetoworkspacesilent", &format!("{MINIMIZED},{window}")],
        );
        // The focus would otherwise stay with the hidden window.
        if let Some(ws) = from {
            dispatch(&format!(r#"hl.dsp.focus({{ workspace = "{ws}" }})"#), &["workspace", &ws.to_string()]);
        }
    });
    true
}

/// A dialog of this process (the stand-in for a Mac password prompt) takes the keyboard: the
/// Sweeflow panel, which keeps the focus while it's open (stay_focused), is closed first, and the
/// dialog focused and put on top. Without that, keys went to the Mac window beneath it.
pub fn take_focus(title: &str) {
    for w in clients_where(|c| c["class"].as_str() == Some("sweeflow") && c["title"].as_str() == Some("Sweeflow")) {
        if let Some(a) = w["address"].as_str() {
            let window = format!("address:{a}");
            dispatch(&format!(r#"hl.dsp.window.close({{ window = "{window}" }})"#), &["closewindow", &window]);
        }
    }
    let Some(c) = our_client("sweeflow", title) else { return };
    let Some(address) = c["address"].as_str() else { return };
    let window = format!("address:{address}");
    dispatch(&format!(r#"hl.dsp.focus({{ window = "{window}" }})"#), &["focuswindow", &window]);
    dispatch(&format!(r#"hl.dsp.window.alter_zorder({{ mode = "top", window = "{window}" }})"#), &["alterzorder", &format!("top,{window}")]);
}

/// A sheet or dialog of a Mac window (it has a parent here): on top and focused once Hyprland has
/// it. Hyprland left a tiled window's sheet under floating windows, its buttons hidden.
pub fn bring_up(class: String, title: String) {
    std::thread::spawn(move || {
        for _ in 0..10 {
            if let Some(c) = our_client(&class, &title) {
                if let Some(address) = c["address"].as_str() {
                    let window = format!("address:{address}");
                    dispatch(&format!(r#"hl.dsp.window.alter_zorder({{ mode = "top", window = "{window}" }})"#), &["alterzorder", &format!("top,{window}")]);
                    dispatch(&format!(r#"hl.dsp.focus({{ window = "{window}" }})"#), &["focuswindow", &window]);
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
}

/// Every Hyprland window (any process) that `pick` says.
fn clients_where(pick: impl Fn(&serde_json::Value) -> bool) -> Vec<serde_json::Value> {
    let Some(out) = Command::new("hyprctl").args(["-j", "clients"]).output().ok() else { return Vec::new() };
    match serde_json::from_slice(&out.stdout) {
        Ok(serde_json::Value::Array(clients)) => clients.into_iter().filter(|c| pick(c)).collect(),
        _ => Vec::new(),
    }
}

/// Brings this process's window of that class back from the "minimized" workspace to the one
/// in front, on top and focused, as Omarchy's dock does: a minimised one first, else the one
/// with that title. For a launch of the app.
pub fn unminimise(class: String, title: String) {
    std::thread::spawn(move || {
        let minimised = our_client_where(&class, |c| c["workspace"]["name"].as_str() == Some(MINIMIZED));
        let Some(c) = minimised.filter(|c| c["workspace"]["name"].as_str() == Some(MINIMIZED)).or_else(|| our_client(&class, &title)) else { return };
        let Some(address) = c["address"].as_str() else { return };
        let window = format!("address:{address}");
        if c["workspace"]["name"].as_str() == Some(MINIMIZED) {
            let Ok(out) = Command::new("hyprctl").args(["-j", "activeworkspace"]).output() else { return };
            let Some(ws) = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok().and_then(|w| w["id"].as_i64()).filter(|&w| w >= 1) else { return };
            dispatch(
                &format!(r#"hl.dsp.window.move({{ workspace = "{ws}", window = "{window}" }})"#),
                &["movetoworkspace", &format!("{ws},{window}")],
            );
        }
        dispatch(&format!(r#"hl.dsp.focus({{ window = "{window}" }})"#), &["focuswindow", &window]);
        dispatch(&format!(r#"hl.dsp.window.alter_zorder({{ mode = "top", window = "{window}" }})"#), &["alterzorder", &format!("top,{window}")]);
    });
}

/// Hyprland frames every window as a plain rectangle (no rounding set, as on Omarchy): a Mac
/// window's rounded corners would leave gaps inside that frame. Looked at once.
pub fn square_windows() -> bool {
    static SQUARE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        running()
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
    if !running() {
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
    if !running() {
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
        let (super_alt, ctrl_alt) = (64 | 8, 4 | 8);
        assert_eq!(super::taken_by(&bound, super_alt, "K").as_deref(), Some("Tmux keybindings"));
        assert_eq!(super::taken_by(&bound, super_alt, "P"), None);
        assert_eq!(super::taken_by(&bound, ctrl_alt, "P"), None);
        assert_eq!(super::taken_by(&bound, ctrl_alt, "K").as_deref(), Some("another binding"));
        assert!(super::parse_binds("not json").is_empty());
    }

    #[test]
    fn uninstall_lets_go_of_our_keys_only() {
        let json = r#"[
            {"modmask": 72, "key": "O", "submap": "", "mouse": false, "description": "Sweeflow: Open Sweeflow"},
            {"modmask": 12, "key": "o", "submap": "", "mouse": false, "description": "Sweeflow: Open Sweeflow"},
            {"modmask": 72, "key": "Y", "submap": "", "mouse": false, "description": "Sweeflow: Pause or resume streaming"},
            {"modmask": 72, "key": "Z", "submap": "", "mouse": false, "description": "Sweeflow: Stop or start streaming"},
            {"modmask": 72, "key": "z", "submap": "", "mouse": false, "description": "My own"},
            {"modmask": 72, "key": "K", "submap": "", "mouse": false, "description": "Tmux keybindings"},
            {"modmask": 72, "key": "mouse:272", "submap": "", "mouse": true, "description": ""}
        ]"#;
        let (lua, n) = super::unbind_lua(json);
        assert_eq!(n, 3);
        assert_eq!(lua, r#"hl.unbind("SUPER + ALT + O"); hl.unbind("CTRL + ALT + O"); hl.unbind("SUPER + ALT + Y"); "#);
        assert_eq!(super::unbind_lua("[]"), (String::new(), 0));
        assert_eq!(super::mods_of(64 | 8).as_deref(), Some("SUPER + ALT"));
        assert_eq!(super::mods_of(4 | 8).as_deref(), Some("CTRL + ALT"));
        assert_eq!(super::mods_of(0), None);
        assert_eq!(super::mods_of(64 | 128), None);
    }
}

/// While a Mac window is focused and hovered, Hyprland is put in a submap of Sweeflow's own:
/// none of the desktop's bindings apply there, so shortcuts with a letter reach the Mac (Super
/// as Cmd), and only the keys for windows and the desktop are bound in it, to what Omarchy has on
/// them: workspaces (Super+1…0 to switch, Super+Shift+1…0 to move the window there), the next
/// and previous workspace (Super+Tab, Super+Shift+Tab), switching windows (Alt+Tab), media and
/// volume, and on Omarchy its menus on Super+Space (with Alt, Ctrl, Ctrl+Shift). The pointer off a Mac window puts the desktop's own bindings back. (Hyprland runs no
/// binding while an app holds its shortcuts, nor one asked for by name: the way other desktops
/// do it doesn't work here.)
pub fn mac_keys(on: bool) {
    std::thread::spawn(move || {
        // Omarchy's menus, where it's Omarchy (Hyprland's Lua can't look for the command).
        let omarchy = Command::new("sh").args(["-c", "command -v omarchy-menu"]).output().is_ok_and(|o| o.status.success());
        let omarchy = if omarchy {
            r#"hl.bind("SUPER + SPACE", hl.dsp.exec_cmd("omarchy-menu toggle root"))
  hl.bind("SUPER + ALT + SPACE", hl.dsp.exec_cmd("omarchy-shell omarchy.dock apps"))
  hl.bind("SUPER + CTRL + SPACE", hl.dsp.exec_cmd("omarchy-menu toggle background"))
  hl.bind("SUPER + SHIFT + CTRL + SPACE", hl.dsp.exec_cmd("omarchy-menu toggle theme"))"#
        } else {
            ""
        };
        // The person's own keys for Linux (deskkeys::UserKey): what Hyprland runs on them, bound
        // in the key mode too. Only programs it starts (exec): the other dispatchers have no
        // name to call here.
        let own = own_binds();
        // Each bind with its name (keptkeys::hyprland's): the ones the person took out in the
        // Shortcuts window aren't bound, and in a Mac window those keys go to the Mac.
        let mut binds: Vec<(String, String)> = Vec::new();
        for n in 1..=10 {
            let (digit, code) = (n % 10, n + 9);
            binds.push((format!("Super+{digit}"), format!(r#"hl.bind("SUPER + code:{code}", hl.dsp.focus({{ workspace = {n} }}))"#)));
            binds.push((format!("Super+Shift+{digit}"), format!(r#"hl.bind("SUPER + SHIFT + code:{code}", hl.dsp.window.move({{ workspace = {n} }}))"#)));
        }
        binds.push(("Super+Tab".into(), r#"hl.bind("SUPER + TAB", hl.dsp.focus({ workspace = "m+1" }))"#.into()));
        binds.push(("Super+Shift+Tab".into(), r#"hl.bind("SUPER + SHIFT + TAB", hl.dsp.focus({ workspace = "m-1" }))"#.into()));
        binds.push(("Alt+Tab".into(), r#"hl.bind("ALT + TAB", hl.dsp.window.cycle_next())"#.into()));
        binds.push(("Alt+Shift+Tab".into(), r#"hl.bind("ALT + SHIFT + TAB", hl.dsp.window.cycle_next({ next = false }))"#.into()));
        for (name, line) in omarchy.lines().zip(["Super+Space", "Super+Alt+Space", "Super+Ctrl+Space", "Super+Ctrl+Shift+Space"]).map(|(l, n)| (n, l.trim())) {
            binds.push((name.into(), line.into()));
        }
        for (name, line) in [
            ("Volume Up", r#"hl.bind("XF86AudioRaiseVolume", hl.dsp.exec_cmd("wpctl set-volume -l 1 @DEFAULT_AUDIO_SINK@ 5%+"), { locked = true, repeating = true })"#),
            ("Volume Down", r#"hl.bind("XF86AudioLowerVolume", hl.dsp.exec_cmd("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"), { locked = true, repeating = true })"#),
            ("Mute", r#"hl.bind("XF86AudioMute", hl.dsp.exec_cmd("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"), { locked = true })"#),
            ("Play/Pause", r#"hl.bind("XF86AudioPlay", hl.dsp.exec_cmd("playerctl play-pause"), { locked = true })"#),
            ("Play/Pause", r#"hl.bind("XF86AudioPause", hl.dsp.exec_cmd("playerctl play-pause"), { locked = true })"#),
            ("Next", r#"hl.bind("XF86AudioNext", hl.dsp.exec_cmd("playerctl next"), { locked = true })"#),
            ("Previous", r#"hl.bind("XF86AudioPrev", hl.dsp.exec_cmd("playerctl previous"), { locked = true })"#),
            ("Brightness Up", r#"hl.bind("XF86MonBrightnessUp", hl.dsp.exec_cmd("brightnessctl set 5%+"), { locked = true, repeating = true })"#),
            ("Brightness Down", r#"hl.bind("XF86MonBrightnessDown", hl.dsp.exec_cmd("brightnessctl set 5%-"), { locked = true, repeating = true })"#),
        ] {
            binds.push((name.into(), line.into()));
        }
        let mut body: String = binds.iter().filter(|(name, _)| !crate::deskkeys::key_removed(name)).map(|(_, line)| format!("{line}\n  ")).collect();
        body += &own;
        // Made again whenever what's in it changes (a version of it, from its text).
        let version = format!("4-{:x}", body.len() * 31 + body.bytes().map(|b| b as usize).sum::<usize>());
        let lua = if on {
            // A key mode of its own for each version: define_submap adds to one that exists
            // already, so a key taken out of the list stayed bound (copies piled up), and
            // hl.unbind can't clear it (it unbinds the key in every mode, the person's own
            // bindings included). Only the mode switched to is used.
            // Each defined once (a version gone back to is there already: defined again it
            // would have every key twice).
            format!(r#"sweeflow_modes = sweeflow_modes or {{}}; if not sweeflow_modes["{version}"] then sweeflow_modes["{version}"] = true; hl.define_submap("sweeflow-{version}", function()
  {body}end) end; hl.dispatch(hl.dsp.submap("sweeflow-{version}"))"#)

        } else {
            r#"hl.dispatch(hl.dsp.submap("reset"))"#.to_string()
        };
        match Command::new("hyprctl").args(["eval", &lua]).output() {
            Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "ok" => {}
            Ok(o) => log::warn!("Hyprland didn't take Sweeflow's key mode ({on}): {}", String::from_utf8_lossy(&o.stdout).trim()),
            Err(e) => log::debug!("hyprctl: {e}"),
        }
    });
}

/// The keys kept for Linux changed: the key mode is made again, now if it's on (made again
/// anyway the next time a Mac window takes the keys: its text changed).
pub fn refresh_mac_keys() {
    if running() && current_submap().is_some_and(|s| s.starts_with("sweeflow")) {
        mac_keys(true);
    }
}

fn current_submap() -> Option<String> {
    let o = Command::new("hyprctl").args(["submap"]).output().ok()?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Lua binds for the person's own keys for Linux, from what Hyprland has on them (`hyprctl binds`):
/// the programs they start. Empty with none.
fn own_binds() -> String {
    let keys: Vec<crate::deskkeys::UserKey> =
        crate::deskkeys::load_user_keys().iter().filter_map(|k| crate::deskkeys::UserKey::parse(k)).collect();
    if keys.is_empty() {
        return String::new();
    }
    let Ok(out) = Command::new("hyprctl").args(["binds", "-j"]).output() else { return String::new() };
    let binds: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let lua_str = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
    let mut lua = String::new();
    for k in &keys {
        let mask = (k.shift as u64) | ((k.ctrl as u64) << 2) | ((k.alt as u64) << 3) | ((k.logo as u64) << 6);
        let name = hypr_key(&k.key);
        let found = binds.iter().find(|b| {
            b["modmask"].as_u64() == Some(mask)
                && b["key"].as_str().is_some_and(|key| key.eq_ignore_ascii_case(&name))
                && b["submap"].as_str().unwrap_or("").is_empty()
        });
        match found.map(|b| (b["dispatcher"].as_str().unwrap_or(""), b["arg"].as_str().unwrap_or(""))) {
            Some(("exec", cmd)) => {
                let mods: Vec<&str> = [(k.logo, "SUPER"), (k.ctrl, "CTRL"), (k.alt, "ALT"), (k.shift, "SHIFT")]
                    .iter()
                    .filter(|m| m.0)
                    .map(|m| m.1)
                    .collect();
                let combo = if mods.is_empty() { name.clone() } else { format!("{} + {name}", mods.join(" + ")) };
                lua += &format!("hl.bind(\"{}\", hl.dsp.exec_cmd(\"{}\"))\n  ", lua_str(&combo), lua_str(cmd));
            }
            Some((other, _)) => log::info!("{}: Hyprland's {other} can't be kept for Linux over Mac windows", k.label()),
            None => log::info!("{}: Hyprland has nothing on it", k.label()),
        }
    }
    lua
}

/// A key's name as Hyprland writes it.
fn hypr_key(key: &str) -> String {
    match key {
        "Enter" => "Return".into(),
        "Esc" => "Escape".into(),
        "PageUp" => "Prior".into(),
        "PageDown" => "Next".into(),
        "Backspace" => "BackSpace".into(),
        "ScrollLock" => "Scroll_Lock".into(),
        other => other.into(),
    }
}

/// Omarchy (its menu command is there).
pub fn omarchy() -> bool {
    Command::new("sh").args(["-c", "command -v omarchy-menu"]).output().is_ok_and(|o| o.status.success())
}
