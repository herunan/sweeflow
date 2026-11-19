//! The update window (`sweeflow update-window`): from the tray's Check for Updates and the
//! panel's update banner. It looks for the latest release, says when this is the latest, and
//! otherwise shows what's new and updates it: install.sh's copy right here (downloaded, checked
//! against the signed checksums, put in place, Sweeflow restarted), a packaged copy by saying the
//! command that updates it. A process of its own, like the pairing wizard, so the streaming
//! client restarting under it (the update) doesn't close it.
//!
//!   sweeflow update-window [--preview checking|latest|failed|available|system|installing|done]

use anyhow::Result;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::update::{self, How};

slint::slint! {
    import { Look, Btn, Dim, Spin } from "look.slint";
    export { Look }

    // One line of the release notes: a bullet ("- …" in CHANGELOG.md) or a paragraph.
    export struct NoteLine { bullet: bool, text: string }

    export component UpdateWindow inherits Window {
        // checking, latest, failed, available, installing, done
        in property <string> state: "checking";
        in property <string> version;
        in property <string> current;
        in property <string> date;
        in property <[NoteLine]> notes;
        // How tall the notes are wrapped to the card (Slint measures wrapped text as one line).
        in property <length> notes-height: 200px;
        // How this copy is updated (update::How): self, system, download.
        in property <string> how: "self";
        in property <string> command;
        in property <string> status;
        in property <string> error;
        in-out property <bool> copied;
        callback update();
        callback dismiss();
        callback retry();
        callback open-page();

        property <bool> has-notes: root.state == "available" || root.state == "installing" || root.state == "done";

        title: "Software Update";
        icon: @image-url("../../assets/sweeflow-app.svg");
        background: Look.bg;
        default-font-family: Look.font;
        width: 520px;
        forward-focus: keys;

        keys := FocusScope {
            key-pressed(event) => {
                if (event.text == Key.Escape && root.state != "installing") {
                    root.dismiss();
                    return accept;
                }
                return reject;
            }
        }

        // As tall as what it shows, and not resizable (a tiling desktop leaves it floating).
        height: main.preferred-height;
        main := VerticalLayout {
            padding: 24px;
            spacing: 18px;
            alignment: start;

            // What this is: the app, the release and where it stands.
            HorizontalLayout {
                spacing: 16px;
                Image {
                    source: @image-url("../../assets/sweeflow-app.svg");
                    width: 56px;
                    height: 56px;
                }
                VerticalLayout {
                    alignment: center;
                    spacing: 4px;
                    Text {
                        text: root.state == "checking" ? "Checking for updates…"
                            : root.state == "latest" ? "Sweeflow is up to date"
                            : root.state == "failed" ? "Couldn't check for updates"
                            : root.state == "done" ? ("Sweeflow " + root.version + " is installed")
                            : root.state == "installing" ? ("Updating to Sweeflow " + root.version + "…")
                            : ("Sweeflow " + root.version + " is available");
                        color: Look.fg;
                        font-size: 18px;
                        font-weight: 600;
                        wrap: word-wrap;
                    }
                    Dim {
                        text: root.state == "checking" ? "Looking at the latest release."
                            : root.state == "latest" ? ("You have the latest version, " + root.current + ".")
                            : root.state == "failed" ? root.error
                            : root.state == "done" ? "Sweeflow has restarted with it."
                            : root.date != "" ? ("You have " + root.current + "  ·  Released " + root.date) : ("You have " + root.current);
                        font-size: 13px;
                    }
                }
                if root.state == "checking": VerticalLayout {
                    alignment: center;
                    Spin { size: 20px; }
                }
            }

            // What's new.
            if root.has-notes: VerticalLayout {
                spacing: 8px;
                Text {
                    text: "What's new";
                    color: Look.muted;
                    font-size: 12px;
                    font-weight: 600;
                }
                // As tall as the notes, up to a point (then they scroll).
                card := Rectangle {
                    height: min(root.notes-height, 300px);
                    border-radius: Look.radius;
                    background: Look.surface;
                    border-width: Look.outlined ? 1px : 0px;
                    border-color: Look.line;
                    Flickable {
                        content-height: lines.preferred-height;
                        // As wide as the card, so its height is that of the notes wrapped to it.
                        lines := VerticalLayout {
                            width: card.width;
                            padding: 16px;
                            spacing: 9px;
                            alignment: start;
                            for line in root.notes: HorizontalLayout {
                                spacing: 10px;
                                if line.bullet: VerticalLayout {
                                    padding-top: 6px;
                                    alignment: start;
                                    Rectangle {
                                        width: 6px;
                                        height: 6px;
                                        border-radius: 3px;
                                        background: Look.fg;
                                    }
                                }
                                Text {
                                    text: line.text;
                                    color: Look.fg;
                                    font-size: 13px;
                                    wrap: word-wrap;
                                }
                            }
                        }
                    }
                }
            }

            // A packaged copy: its system updates it.
            if root.state == "available" && root.how == "system": VerticalLayout {
                spacing: 8px;
                Dim { text: "Sweeflow comes with your system's updates. In a terminal, run:"; }
                Rectangle {
                    height: 38px;
                    border-radius: Look.radius;
                    background: Look.surface;
                    HorizontalLayout {
                        padding-left: 12px;
                        padding-right: 4px;
                        spacing: 8px;
                        // Selectable, and copied whole by Copy.
                        cmd-text := TextInput {
                            horizontal-stretch: 1;
                            text: root.command;
                            read-only: true;
                            single-line: true;
                            color: Look.fg;
                            selection-background-color: Look.accent.transparentize(0.6);
                            font-family: Look.mono;
                            font-size: 13px;
                            vertical-alignment: center;
                        }
                        VerticalLayout {
                            alignment: center;
                            horizontal-stretch: 0;
                            Btn {
                                small: true;
                                text: root.copied ? "Copied" : "Copy";
                                clicked => {
                                    cmd-text.select-all();
                                    cmd-text.copy();
                                    cmd-text.clear-selection();
                                    root.copied = true;
                                }
                            }
                        }
                    }
                }
            }
            if root.state == "available" && root.how == "download": Dim {
                text: "This copy was installed from a package file. Get the new one from the release page.";
            }

            // Under way: each step, and how far the download is.
            if root.state == "installing": VerticalLayout {
                spacing: 8px;
                Rectangle {
                    height: 6px;
                    border-radius: 3px;
                    background: Look.surface;
                    clip: true;
                    bar := Rectangle {
                        x: 0px;
                        width: parent.width * 0.35;
                        height: parent.height;
                        border-radius: 3px;
                        background: Look.accent;
                    }
                    // No sizes to go by: a band that runs across.
                    Timer {
                        interval: 16ms;
                        running: root.state == "installing";
                        triggered => {
                            bar.x = bar.x > parent.width ? -bar.width : bar.x + 3px;
                        }
                    }
                }
                Dim { text: root.status; font-size: 12px; }
            }

            // The buttons.
            HorizontalLayout {
                spacing: 8px;
                if root.has-notes: VerticalLayout {
                    alignment: center;
                    // A link: the text's colour, underlined.
                    page-link := Text {
                        text: "Release page";
                        color: page-touch.has-hover ? Look.muted : Look.fg;
                        font-size: 13px;
                        page-touch := TouchArea { mouse-cursor: pointer; clicked => { root.open-page(); } }
                        Rectangle {
                            y: parent.height - 1px;
                            width: parent.width;
                            height: 1px;
                            background: page-link.color;
                        }
                    }
                }
                Rectangle { horizontal-stretch: 1; }
                if root.state == "available": Btn { text: root.how == "self" ? "Later" : "Close"; clicked => { root.dismiss(); } }
                if root.state == "available" && root.how == "self": Btn { text: "Update and Restart"; primary: true; clicked => { root.update(); } }
                if root.state == "available" && root.how == "download": Btn { text: "Open Release Page"; primary: true; clicked => { root.open-page(); } }
                if root.state == "failed": Btn { text: "Close"; clicked => { root.dismiss(); } }
                if root.state == "failed": Btn { text: "Try Again"; primary: true; clicked => { root.retry(); } }
                if root.state == "latest" || root.state == "done": Btn { text: "OK"; primary: true; clicked => { root.dismiss(); } }
                if root.state == "checking": Btn { text: "Cancel"; clicked => { root.dismiss(); } }
                if root.state == "installing": Btn { text: "Update and Restart"; primary: true; enabled: false; }
            }
        }
    }
}

/// CHANGELOG.md's markdown as lines: "- " items as bullets, the rest as paragraphs (lines of one
/// paragraph joined), the inline marks (**bold**, `code`) left out.
fn note_lines(md: &str) -> Vec<NoteLine> {
    let plain = |s: &str| SharedString::from(s.replace("**", "").replace('`', "").trim());
    let mut out: Vec<NoteLine> = Vec::new();
    let mut joining = false;
    for line in md.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            joining = false;
            continue;
        }
        if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            out.push(NoteLine { bullet: true, text: plain(item) });
            joining = true;
        } else if joining && !out.is_empty() {
            let last = out.last_mut().unwrap();
            last.text = format!("{} {}", last.text, plain(t)).into();
        } else {
            out.push(NoteLine { bullet: false, text: plain(t) });
            joining = true;
        }
    }
    out
}

/// The notes' height in the card: about 66 characters to a line of 13 px text (18 px tall) in its
/// 440 px, lines 9 px apart, 16 px above and below.
fn notes_height(lines: &[NoteLine]) -> f32 {
    let rows: usize = lines.iter().map(|l| l.text.chars().count().div_ceil(if l.bullet { 64 } else { 68 }).max(1)).sum();
    32.0 + rows as f32 * 18.0 + lines.len().saturating_sub(1) as f32 * 9.0
}

/// "2026-11-19" as "19 Nov 2026".
fn nice_date(iso: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let p: Vec<&str> = iso.split('-').collect();
    match (p.first(), p.get(1).and_then(|m| m.parse::<usize>().ok()), p.get(2).and_then(|d| d.parse::<u32>().ok())) {
        (Some(y), Some(m @ 1..=12), Some(d)) => format!("{d} {} {y}", MONTHS[m - 1]),
        _ => iso.to_string(),
    }
}

fn show_feed(ui: &UpdateWindow, feed: &update::Feed) {
    ui.set_version(feed.version.as_str().into());
    ui.set_date(nice_date(&feed.date).into());
    let lines = note_lines(&feed.notes);
    ui.set_notes_height(notes_height(&lines));
    ui.set_notes(ModelRc::new(VecModel::from(lines)));
    let how = update::how();
    ui.set_how(how.id().into());
    if let How::System(cmd) = how {
        ui.set_command(cmd.into());
    }
    ui.set_state("available".into());
}

/// Looks at the latest release on a thread of its own, and shows what it found.
fn look(ui: &UpdateWindow) {
    ui.set_state("checking".into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let found = update::check_quiet();
        let _ = weak.upgrade_in_event_loop(move |ui| match found {
            Ok(Some(feed)) => show_feed(&ui, &feed),
            Ok(None) => ui.set_state("latest".into()),
            Err(e) => {
                ui.set_error(format!("{e:#}").into());
                ui.set_state("failed".into());
            }
        });
    });
}

pub fn run(args: &[String]) -> Result<()> {
    let ui = UpdateWindow::new()?;
    crate::look::apply!(ui);
    crate::hyprland::keep_window_size();
    crate::look::set_app_id();
    ui.set_current(update::VERSION.into());
    let feed = std::rc::Rc::new(std::cell::RefCell::new(None::<update::Feed>));

    ui.on_dismiss(|| {
        let _ = slint::quit_event_loop();
    });
    ui.window().on_close_requested({
        let weak = ui.as_weak();
        move || {
            // Not while it's putting the new copy in place.
            if weak.upgrade().is_some_and(|ui| ui.get_state() == "installing") {
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        }
    });
    ui.on_retry({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                look(&ui);
            }
        }
    });
    ui.on_open_page({
        let feed = feed.clone();
        move || {
            let page = update::latest().map(|f| f.page).filter(|p| !p.is_empty()).or_else(|| feed.borrow().as_ref().map(|f| f.page.clone()));
            let page = page.unwrap_or_else(|| "https://github.com/herunan/sweeflow/releases/latest".into());
            let _ = std::process::Command::new("xdg-open").arg(page).spawn();
        }
    });
    ui.on_update({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_status("Downloading…".into());
            ui.set_state("installing".into());
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let step = {
                    let weak = weak.clone();
                    move |words: &str| {
                        let words = SharedString::from(words);
                        let _ = weak.upgrade_in_event_loop(move |ui| ui.set_status(words));
                    }
                };
                let done = update::check_quiet().and_then(|f| match f {
                    Some(feed) => update::install_tarball(&feed, &step),
                    None => Ok(()),
                });
                let _ = weak.upgrade_in_event_loop(move |ui| match done {
                    Ok(()) => ui.set_state("done".into()),
                    Err(e) => {
                        ui.set_error(format!("{e:#}").into());
                        ui.set_state("failed".into());
                    }
                });
            });
        }
    });

    match args.iter().position(|a| a == "--preview").and_then(|i| args.get(i + 1)).map(String::as_str) {
        Some(state) => preview(&ui, state),
        None => look(&ui),
    }
    ui.run()?;
    Ok(())
}

/// A state with made-up contents, for looking at the window (screenshots).
fn preview(ui: &UpdateWindow, state: &str) {
    let feed = update::Feed {
        version: "0.2.0".into(),
        date: "2026-12-03".into(),
        notes: "Smoother windows, and Sweeflow tells you when a new version is out.\n\n\
                - Mac windows open and move about faster, with less delay when typing.\n\
                - Check for Updates… in the Mac menu and the Linux tray, with this window.\n\
                - The Mac says in a notification when streaming starts, pauses and stops.\n\
                - Shortcuts you keep for Linux now also work while a Mac window is in front.\n\
                - Fixes for sheets on Hyprland and the Alt+Tab switcher on Linux Mint."
            .into(),
        page: String::new(),
        ..Default::default()
    };
    match state {
        "checking" => ui.set_state("checking".into()),
        "latest" => ui.set_state("latest".into()),
        "failed" => {
            ui.set_error("No connection to github.com. Check you're online and try again.".into());
            ui.set_state("failed".into());
        }
        "system" => {
            show_feed(ui, &feed);
            ui.set_how("system".into());
            ui.set_command("sudo pacman -Syu".into());
        }
        "installing" => {
            show_feed(ui, &feed);
            ui.set_status("Checking the download against the release's signature…".into());
            ui.set_state("installing".into());
        }
        "done" => {
            show_feed(ui, &feed);
            ui.set_state("done".into());
        }
        _ => {
            show_feed(ui, &feed);
            ui.set_how("self".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_notes_read_as_lines() {
        let l = note_lines("The first release.\n\n- Mac apps **stream**.\n- Two `keys`\n  carried on.\n\nAfter.");
        let got: Vec<(bool, String)> = l.iter().map(|n| (n.bullet, n.text.to_string())).collect();
        assert_eq!(
            got,
            vec![
                (false, "The first release.".into()),
                (true, "Mac apps stream.".into()),
                (true, "Two keys carried on.".into()),
                (false, "After.".into()),
            ]
        );
        assert_eq!(nice_date("2026-11-19"), "19 Nov 2026");
    }
}
