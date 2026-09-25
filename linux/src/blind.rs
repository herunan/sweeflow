//! Stand-ins for Mac windows that refuse to be captured (security prompts, sign-in verifiers): their
//! text and buttons, read on the Mac through Accessibility, shown as a native KDE dialog. The
//! button clicked here is pressed there. Ones without buttons (Touch ID / password prompts,
//! which must be answered on the Mac itself) become a notification.

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
    let text = texts.join("\n\n");
    if buttons.is_empty() {
        let _ = Command::new("notify-send")
            .args(["--app-name=sweeflow", "--icon=dialog-password", "--urgency=critical",
                   &format!("{heading} needs you on your Mac"), &text])
            .spawn();
        return;
    }
    let mut cmd = Command::new("kdialog");
    cmd.args(["--title", &heading]);
    match buttons.len() {
        1 => {
            cmd.args(["--msgbox", &text, "--ok-label", &buttons[0]]);
        }
        2 => {
            cmd.args(["--yesno", &text, "--yes-label", &buttons[0], "--no-label", &buttons[1]]);
        }
        3 => {
            cmd.args(["--yesnocancel", &text, "--yes-label", &buttons[0], "--no-label", &buttons[1], "--cancel-label", &buttons[2]]);
        }
        _ => {
            cmd.args(["--menu", &text]);
            for (i, b) in buttons.iter().enumerate() {
                cmd.args([i.to_string(), b.clone()]);
            }
        }
    }
    let menu = buttons.len() > 3;
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
        let code = out.status.code().unwrap_or(-1);
        let index = if menu {
            String::from_utf8_lossy(&out.stdout).trim().parse::<u16>().ok()
        } else {
            // kdialog: 0 first button, 1 second, 2 third (closing the dialog counts as the second
            // or third: "No"/"Cancel", as with any dialog).
            (0..3).contains(&code).then_some(code as u16)
        };
        if let Some(i) = index {
            log::info!("stand-in {id}: pressing button {i}");
            let _ = net.send(NetCmd::Control(W::new(t::BLIND_PRESS).u32(id).u16(i).finish()));
        }
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
