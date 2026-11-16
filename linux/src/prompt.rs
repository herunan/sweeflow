//! A Mac prompt that can't be streamed, shown here as a plain dialog (`sweeflow prompt …`, run
//! by blind.rs): its text and its buttons, no icons, as macOS shows them. A process of its own,
//! like the pairing wizard, so its event loop never shares a thread with the streaming client's.
//!
//!   sweeflow prompt DEFAULT CANCEL HEADING TEXT INDEX LABEL [INDEX LABEL …]
//!
//! DEFAULT and CANCEL are button indexes (-1: none); the buttons come in the Mac's order (its
//! default last, so rightmost). Exits with 10 + the index of the button pressed; closing the
//! window (exit 0) answers like the cancel button.

use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, VecModel};

slint::slint! {
    import { Look, Btn } from "look.slint";
    export { Look }

    export struct PromptButton { index: int, label: string }

    export component Prompt inherits Window {
        in property <string> heading;
        in property <string> body;
        in property <[PromptButton]> buttons;
        in property <int> default-index: -1;
        callback pressed(int);
        callback cancelled();

        title: root.heading;
        background: Look.bg;
        default-font-family: Look.font;
        // As big as its text and buttons need, and not resizable (so a tiling desktop leaves it
        // floating, as it does any dialog).
        width: max(480px, row.preferred-width + 40px);
        height: body-text.y + body-text.height + row.height + 44px;
        forward-focus: keys;

        keys := FocusScope {
            key-pressed(event) => {
                if (event.text == Key.Return && root.default-index >= 0) {
                    root.pressed(root.default-index);
                    return accept;
                }
                if (event.text == Key.Escape) {
                    root.cancelled();
                    return accept;
                }
                return reject;
            }
        }
        // The heading (the Mac app, and its prompt's title) inside the window too, as macOS
        // shows it: a tiling desktop (Hyprland) draws no title bar, and it was nowhere to be seen.
        heading-text := Text {
            x: 20px;
            y: 20px;
            width: root.width - 40px;
            height: self.preferred-height;
            text: root.heading;
            color: Look.fg;
            wrap: word-wrap;
            font-size: 15px;
            font-weight: 600;
        }
        body-text := Text {
            x: 20px;
            y: heading-text.y + heading-text.height + 10px;
            width: root.width - 40px;
            height: self.preferred-height;
            text: root.body;
            color: Look.fg;
            wrap: word-wrap;
            font-size: 14px;
        }
        row := HorizontalLayout {
            x: 20px;
            y: body-text.y + body-text.height + 24px;
            width: root.width - 40px;
            height: self.preferred-height;
            alignment: end;
            spacing: 8px;
            for b in root.buttons: Btn {
                text: b.label;
                primary: b.index == root.default-index;
                clicked => { root.pressed(b.index); }
            }
        }
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let number = |i: usize| args.get(i).and_then(|a| a.parse::<i32>().ok()).context("usage: sweeflow prompt DEFAULT CANCEL HEADING TEXT INDEX LABEL …");
    let (default, cancel) = (number(0)?, number(1)?);
    let buttons: Vec<PromptButton> = args
        .get(4..)
        .unwrap_or_default()
        .chunks_exact(2)
        .filter_map(|b| Some(PromptButton { index: b[0].parse().ok()?, label: b[1].as_str().into() }))
        .collect();
    let ui = Prompt::new()?;
    crate::look::apply!(ui);
    crate::hyprland::keep_window_size();
    // Matches sweeflow.desktop (written by `sweeflow install`): Sweeflow's logo in the title bar
    // and the taskbar. After the window is made (there's nothing to set it on before), and
    // before it's shown.
    if let Err(e) = slint::set_xdg_app_id("sweeflow") {
        log::warn!("app id: {e}");
    }
    ui.set_heading(args.get(2).map(String::as_str).unwrap_or_default().into());
    ui.set_body(args.get(3).map(String::as_str).unwrap_or_default().into());
    ui.set_buttons(ModelRc::new(VecModel::from(buttons)));
    ui.set_default_index(default);
    ui.on_pressed(|index| std::process::exit(10 + index));
    ui.on_cancelled(move || std::process::exit(if cancel >= 0 { 10 + cancel } else { 0 }));
    ui.run()?;
    Ok(())
}
