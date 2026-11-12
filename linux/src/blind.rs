//! Stand-ins for Mac windows that refuse to be captured (security prompts, sign-in verifiers): their
//! text and buttons, read on the Mac through Accessibility, shown as a plain dialog (prompt.qml:
//! text and buttons, no icons, as macOS shows them; kdialog where Qt's `qml` runner isn't
//! installed). The button clicked here is pressed there. Ones without buttons (Touch ID /
//! password prompts, which must be answered on the Mac itself) become a notification.

use std::{collections::HashMap, process::Command, sync::Mutex};

use tokio::sync::mpsc::UnboundedSender;

use crate::{
    net::NetCmd,
    proto::{t, W},
};

/// The dialog (process id) shown for each window, closed when the Mac window goes away.
static DIALOGS: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

pub fn show(id: u32, app: &str, title: &str, texts: &[String], buttons: &[String], net: UnboundedSender<NetCmd>) {
    let heading = if title.is_empty() || title == app { app.to_string() } else { format!("{app} — {title}") };
    // "Help" only opens Apple's help viewer; the rest keep their Mac order (default last).
    let shown: Vec<(usize, &String)> = buttons.iter().enumerate().filter(|(_, b)| b.as_str() != "Help").collect();
    if shown.is_empty() {
        let _ = Command::new("notify-send")
            .args(["--app-name=sweeflow", "--icon=dialog-password", "--urgency=critical",
                   &format!("{heading} needs you on your Mac"), &texts.join("\n\n")])
            .spawn();
        return;
    }
    // The Mac's cancel button (closing the dialog answers like it, as with any dialog).
    let cancel = buttons.iter().position(|b| ["Cancel", "Deny", "Don’t Allow", "Don't Allow", "No"].contains(&b.as_str()));
    let text = texts.join("\n\n");
    if let Some(child) = spawn_prompt(&heading, &text, &shown, cancel) {
        let pid = child.id();
        DIALOGS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, pid);
        let (app, title, texts, buttons) = (app.to_string(), title.to_string(), texts.to_vec(), buttons.to_vec());
        std::thread::spawn(move || {
            let Ok(status) = child.wait_with_output().map(|o| o.status) else { return };
            // Closed because the Mac window went away (or replaced): nothing to press.
            let ours = DIALOGS.lock().unwrap().as_mut().and_then(|m| (m.get(&id) == Some(&pid)).then(|| m.remove(&id))).flatten();
            if ours.is_none() {
                return;
            }
            // 10 + the button pressed; 0 = the window was closed, which answers like cancel.
            let index = match status.code() {
                Some(code) if code >= 10 => Some((code - 10) as usize),
                Some(0) => cancel,
                // It didn't come up (no such style, a Qt without Quick Controls): kdialog instead.
                Some(code) => {
                    log::warn!("stand-in dialog: the plain prompt failed ({code}), using kdialog");
                    return show_kdialog(id, &app, &title, &texts, &buttons, net);
                }
                // Killed: nothing to press.
                None => return,
            };
            if let Some(i) = index {
                log::info!("stand-in {id}: pressing button {i}");
                let _ = net.send(NetCmd::Control(W::new(t::BLIND_PRESS).u32(id).u16(i as u16).finish()));
            }
        });
        return;
    }
    show_kdialog(id, app, title, texts, buttons, net);
}

/// The plain prompt (prompt.qml), run by Qt's `qml` tool, whatever this system calls it.
/// None when it isn't installed.
fn spawn_prompt(heading: &str, text: &str, shown: &[(usize, &String)], cancel: Option<usize>) -> Option<std::process::Child> {
    static QML: &str = include_str!("prompt.qml");
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let file = dir.join("sweeflow-prompt.qml");
    std::fs::write(&file, QML).ok()?;
    // The Mac's default button is its last.
    let default = shown.last().map(|(i, _)| *i as i64).unwrap_or(-1);
    for tool in ["qml6", "qml-qt6", "/usr/lib/qt6/bin/qml", "/usr/lib64/qt6/bin/qml"] {
        let mut cmd = Command::new(tool);
        // A widgets application, so the desktop's own style (Breeze) draws the buttons.
        cmd.args(["--apptype", "widget"]).arg(&file).arg("--");
        cmd.args([default.to_string(), cancel.map_or(-1, |c| c as i64).to_string(), heading.to_string(), text.to_string()]);
        for (i, label) in shown {
            cmd.args([i.to_string(), label.to_string()]);
        }
        if let Ok(child) = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            return Some(child);
        }
    }
    None
}

/// The same with kdialog, whose dialogs always carry an icon and icons on their buttons.
fn show_kdialog(id: u32, app: &str, title: &str, texts: &[String], buttons: &[String], net: UnboundedSender<NetCmd>) {
    let heading = if title.is_empty() || title == app { app.to_string() } else { format!("{app} — {title}") };
    let shown: Vec<(usize, &String)> = buttons.iter().enumerate().filter(|(_, b)| b.as_str() != "Help").collect();
    let cancel = buttons.iter().position(|b| ["Cancel", "Deny", "Don’t Allow", "Don't Allow", "No"].contains(&b.as_str()));
    let text = texts.join("\n\n");
    // kdialog's buttons carry stock icons (tick, prohibited sign), so the Mac's cancel button
    // goes where the prohibited sign is and the others where a tick or arrow fits.
    let mut cmd = Command::new("kdialog");
    cmd.args(["--title", &heading]);
    let others: Vec<usize> = shown.iter().map(|(i, _)| *i).filter(|i| Some(*i) != cancel).collect();
    let label = |i: usize| buttons[i].as_str();
    // Which Mac button each kdialog exit code (0, 1, 2) stands for.
    let mut codes: Vec<Option<usize>> = vec![None; 3];
    let menu = match (others.as_slice(), cancel) {
        ([only], None) => {
            cmd.args(["--msgbox", &text, "--ok-label", label(*only)]);
            codes[0] = Some(*only);
            false
        }
        ([go], Some(c)) => {
            cmd.args(["--warningcontinuecancel", &text, "--continue-label", label(*go), "--cancel-label", label(c)]);
            codes[0] = Some(*go);
            codes[2] = Some(c);
            false
        }
        ([a, b], None) => {
            cmd.args(["--yesno", &text, "--yes-label", label(*b), "--no-label", label(*a)]);
            codes[0] = Some(*b);
            codes[1] = Some(*a);
            false
        }
        ([a, b], Some(c)) => {
            cmd.args(["--yesnocancel", &text, "--yes-label", label(*b), "--no-label", label(*a), "--cancel-label", label(c)]);
            codes[0] = Some(*b);
            codes[1] = Some(*a);
            codes[2] = Some(c);
            false
        }
        _ => {
            cmd.args(["--menu", &text]);
            for (i, b) in &shown {
                cmd.args([i.to_string(), b.to_string()]);
            }
            true
        }
    };
    let child = match cmd.stdout(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("stand-in dialog: {e}");
            return;
        }
    };
    let pid = child.id();
    DIALOGS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, pid);
    std::thread::spawn(move || {
        let Ok(out) = child.wait_with_output() else { return };
        // Closed because the Mac window went away (or replaced): nothing to press.
        let ours = DIALOGS.lock().unwrap().as_mut().and_then(|m| (m.get(&id) == Some(&pid)).then(|| m.remove(&id))).flatten();
        if ours.is_none() {
            return;
        }
        // Killed or crashed (no exit code): nothing to press.
        let Some(code) = out.status.code() else { return };
        let index = if menu {
            String::from_utf8_lossy(&out.stdout).trim().parse::<usize>().ok()
        } else {
            codes.get(code as usize).copied().flatten()
        };
        if let Some(i) = index {
            log::info!("stand-in {id}: pressing button {i}");
            let _ = net.send(NetCmd::Control(W::new(t::BLIND_PRESS).u32(id).u16(i as u16).finish()));
        }
    });
}

/// An uncapturable Mac menu (a pop-up button's): its items as a pick-list. Cancelling closes
/// the menu on the Mac.
pub fn show_menu(id: u32, app: &str, items: &[String], net: UnboundedSender<NetCmd>) {
    let mut cmd = Command::new("kdialog");
    cmd.args(["--title", app, "--menu", "Choose:"]);
    for (i, item) in items.iter().enumerate() {
        cmd.args([i.to_string(), item.clone()]);
    }
    let child = match cmd.stdout(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return log::warn!("stand-in menu: {e}"),
    };
    let pid = child.id();
    DIALOGS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, pid);
    std::thread::spawn(move || {
        let Ok(out) = child.wait_with_output() else { return };
        let ours = DIALOGS.lock().unwrap().as_mut().and_then(|m| (m.get(&id) == Some(&pid)).then(|| m.remove(&id))).flatten();
        if ours.is_none() {
            return;
        }
        // Killed (no exit code): nothing to do. Otherwise a choice, or cancelled (0xFFFF).
        let Some(code) = out.status.code() else { return };
        let index = if code == 0 { String::from_utf8_lossy(&out.stdout).trim().parse::<u16>().ok() } else { None };
        let index = index.unwrap_or(0xFFFF);
        log::info!("stand-in menu {id}: {}", if index == 0xFFFF { "cancelled".to_string() } else { format!("item {index}") });
        let _ = net.send(NetCmd::Control(W::new(t::BLIND_PRESS).u32(id).u16(index).finish()));
    });
}

/// The Mac window went away: close its dialog.
pub fn gone(id: u32) {
    if let Some(pid) = DIALOGS.lock().unwrap().as_mut().and_then(|m| m.remove(&id)) {
        // SAFETY: signals a process we started (removed from the map, so the waiting thread
        // won't press anything).
        unsafe { kill(pid as i32, 15) };
    }
}
