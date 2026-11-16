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
    if !running() {
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
    let bin = crate::setup::installed_binary(std::path::Path::new(&home)).display().to_string();
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
        assert_eq!(super::bound_by(&bound, super::SUPER_ALT, 'K').as_deref(), Some("Tmux keybindings"));
        assert_eq!(super::bound_by(&bound, super::SUPER_ALT, 'P'), None);
        assert_eq!(super::bound_by(&bound, super::CTRL_ALT, 'P'), None);
        assert_eq!(super::bound_by(&bound, super::CTRL_ALT, 'K').as_deref(), Some("another binding"));
        assert!(super::parse_binds("not json").is_empty());
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
        let lua = if on {
            r#"if sweeflow_submap ~= 3 then sweeflow_submap = 3; hl.define_submap("sweeflow", function()
  for n = 1, 10 do
    local key = "code:" .. tostring(n + 9)
    hl.bind("SUPER + " .. key, hl.dsp.focus({ workspace = n }))
    hl.bind("SUPER + SHIFT + " .. key, hl.dsp.window.move({ workspace = n }))
  end
  hl.bind("SUPER + TAB", hl.dsp.focus({ workspace = "m+1" }))
  hl.bind("SUPER + SHIFT + TAB", hl.dsp.focus({ workspace = "m-1" }))
  hl.bind("ALT + TAB", hl.dsp.window.cycle_next())
  hl.bind("ALT + SHIFT + TAB", hl.dsp.window.cycle_next({ next = false }))
  OMARCHY
  hl.bind("XF86AudioRaiseVolume", hl.dsp.exec_cmd("wpctl set-volume -l 1 @DEFAULT_AUDIO_SINK@ 5%+"), { locked = true, repeating = true })
  hl.bind("XF86AudioLowerVolume", hl.dsp.exec_cmd("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"), { locked = true, repeating = true })
  hl.bind("XF86AudioMute", hl.dsp.exec_cmd("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"), { locked = true })
  hl.bind("XF86AudioPlay", hl.dsp.exec_cmd("playerctl play-pause"), { locked = true })
  hl.bind("XF86AudioPause", hl.dsp.exec_cmd("playerctl play-pause"), { locked = true })
  hl.bind("XF86AudioNext", hl.dsp.exec_cmd("playerctl next"), { locked = true })
  hl.bind("XF86AudioPrev", hl.dsp.exec_cmd("playerctl previous"), { locked = true })
  hl.bind("XF86MonBrightnessUp", hl.dsp.exec_cmd("brightnessctl set 5%+"), { locked = true, repeating = true })
  hl.bind("XF86MonBrightnessDown", hl.dsp.exec_cmd("brightnessctl set 5%-"), { locked = true, repeating = true })
end) end; hl.dispatch(hl.dsp.submap("sweeflow"))"#
                .replace("OMARCHY", omarchy)
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
