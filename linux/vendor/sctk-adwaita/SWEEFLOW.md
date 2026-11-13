# Why this copy is here

This is sctk-adwaita 0.10.1 from crates.io (MIT, see LICENSE) with two changes, both marked
"Sweeflow" in the source:

- `src/lib.rs`: a frame starts as not resizable (upstream: resizable).
- `src/pointer.rs`: a double click on the title bar only maximises a resizable window.

The library draws the title bar of Sweeflow's own windows (Add a Mac, a Mac's prompt) on GNOME,
which leaves that to each app. Those windows have a fixed size, but a double click on the title
bar maximised them anyway, and GNOME went along with it: the window filled the screen with its
content in a corner. Two things caused it. winit (0.30.13) only tells the frame "not resizable"
when that changes after the frame exists, so a window that is fixed from the start never got it
through; and upstream doesn't look at that flag for the double click in any case.

Every window Sweeflow opens through winit has a fixed size, so starting from "not resizable" is
right here. It would be wrong for an app with resizable windows.

`linux/Cargo.toml` points at this folder under `[patch.crates-io]`. To update: copy the new
version here and reapply the two changes, or drop the patch once winit and sctk-adwaita handle it.
