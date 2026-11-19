<div align="center">

<img src="site/icon.svg" width="96" alt="Sweeflow logo">

# Sweeflow

### A Linux home for your Mac apps

Are you Linux at heart, with one Mac app you can't drop?<br>
Keep both on one screen.

[**Website**](https://sweeflow.app) · [**Download**](https://github.com/herunan/sweeflow/releases/latest) · [**Sponsor**](https://github.com/sponsors/herunan)

[![MIT licence](https://img.shields.io/badge/licence-MIT-8b5cf6)](LICENSE)
![Linux client](https://img.shields.io/badge/Linux-Wayland%20%26%20X11-8b5cf6)
![Mac host](https://img.shields.io/badge/macOS-15%2B-8b5cf6)

<img src="site/og.png" width="820" alt="Mac apps in their own windows on a Linux desktop, next to Konsole and Dolphin">

</div>

Sweeflow streams the windows of your Mac apps to your Linux desktop, one window each. They sit next to your Linux windows in the taskbar, tile and snap with them, and take your keyboard and mouse like any other window. No remote desktop, no second screen to look at.

## What it does

- 🔗 **Link your Mac.** Choose it on Linux, type the 4-digit code it shows, and you're streaming. Encrypted, with each computer's certificate pinned when you link them.
- 🚀 **Open Mac apps** from your Linux launcher, next to your Linux apps.
- 🪟 **Use them like any window.** Move, resize, minimise, close, tile. Smooth at up to 120 fps, and Retina sharp.
- 🤝 **Both working together.** Drag files across, copy and paste between apps, hear the Mac's sound on Linux.
- ⌨️ **Mac shortcuts where they belong.** ⌘ shortcuts go to the Mac window you're in; your desktop keeps its own (window switching, workspaces, launcher), and you choose which.
- 🌍 **Your Mac, anywhere.** A MacBook beside you, a Mac mini miles away over Tailscale, or a macOS VM on your PC.

## Your Linux flavour

Tested on **KDE Plasma**, **GNOME** (Ubuntu, CachyOS), **Cinnamon** (Linux Mint) and **Hyprland** (Omarchy), on Wayland and X11.

## Install

Two apps: the **host** goes on your Mac and shares its windows; the **client** goes on Linux and shows them.

### On Linux

Any distro:

```sh
curl -fsSL https://github.com/herunan/sweeflow/releases/latest/download/install.sh | sh
```

Arch (AUR):

```sh
yay -S sweeflow
```

Debian, Ubuntu, Mint:

```sh
curl -fsSLO https://github.com/herunan/sweeflow/releases/latest/download/sweeflow_amd64.deb
sudo apt install ./sweeflow_amd64.deb
```

Fedora and other RPM distros: `sweeflow.x86_64.rpm` from the [latest release](https://github.com/herunan/sweeflow/releases/latest).

### On your Mac

[**Download Sweeflow for Mac**](https://github.com/herunan/sweeflow/releases/latest/download/Sweeflow.dmg) (macOS 15 or later), open it and drag Sweeflow to Applications. Its setup asks for the permissions it needs (Screen Recording, Accessibility) and ends on the **Link a Linux machine** screen.

### Link them

On Linux, Sweeflow's setup opens by itself: choose your Mac and type the code it shows. That's it.

## How it works

The Mac host captures each window with ScreenCaptureKit and encodes it as HEVC (10-bit colour in High quality). The Linux client decodes with VA-API on the GPU (or on the CPU where it can't) and shows each Mac window as a window of its own, in a Wayland or X11 client written in Rust. Pictures, input, clipboard, files and sound travel over QUIC. See [PROTOCOL.md](PROTOCOL.md) for the details.

## Build from source

```sh
# Linux client (Rust)
cd linux && cargo build --release && ./target/release/sweeflow install

# Mac host (Swift, on a Mac)
cd mac && ./install.sh
```

## Support

Sweeflow is free and open source under the [MIT licence](LICENSE). If it's useful to you, you can help keep it going:

[![Sponsor on GitHub](https://img.shields.io/badge/Sponsor-GitHub-8b5cf6?logo=githubsponsors&logoColor=white)](https://github.com/sponsors/herunan)
[![Buy me a coffee](https://img.shields.io/badge/Buy%20me%20a%20coffee-Ko--fi-8b5cf6?logo=kofi&logoColor=white)](https://ko-fi.com/herunan)

<sub>Mac and macOS are trademarks of Apple Inc. Sweeflow is not affiliated with Apple.</sub>
