#!/bin/sh
# Sweeflow for any Linux distro, without root:
#   curl -fsSL https://github.com/herunan/sweeflow/releases/latest/download/install.sh | sh
# Downloads the release's sweeflow-x86_64.tar.gz, checks it against the release's SHA256SUMS, and runs
# `sweeflow install`, which copies it to ~/.local/bin, starts it with the desktop (a systemd user
# service), and opens the "Link a Mac" window. Run it again to update. To remove Sweeflow:
# `sweeflow uninstall`.
# SWEEFLOW_RELEASE=https://.../download can point it at another release's files.
set -eu
release="${SWEEFLOW_RELEASE:-https://github.com/herunan/sweeflow/releases/latest/download}"

say() { printf '%s\n' "$*"; }
fail() { say "sweeflow: $*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || fail "this installer is for Linux; on a Mac, download Sweeflow from https://sweeflow.app"
[ "$(uname -m)" = x86_64 ] || fail "Sweeflow is built for x86_64 (64-bit Intel/AMD) only so far, not $(uname -m)"
[ "$(id -u)" != 0 ] || fail "run this as yourself, not as root: Sweeflow installs for your user"
command -v systemctl >/dev/null || fail "Sweeflow runs as a systemd user service, and this system has no systemd"
if [ -x /usr/bin/sweeflow ]; then
    fail "a package already installed Sweeflow (/usr/bin/sweeflow): update it with your package manager instead"
fi
glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')
case "$glibc" in
    2.[0-9]|2.[0-2][0-9]|2.3[0-4]) fail "Sweeflow needs glibc 2.35 or later (Ubuntu 22.04, Debian 12, Fedora 36 or newer); this system has $glibc" ;;
esac

if command -v curl >/dev/null; then
    get() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null; then
    get() { wget -q -O "$2" "$1"; }
else
    fail "needs curl or wget to download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
say "==> downloading Sweeflow"
get "$release/sweeflow-x86_64.tar.gz" "$tmp/sweeflow.tar.gz" || fail "download failed: $release/sweeflow-x86_64.tar.gz"
get "$release/SHA256SUMS" "$tmp/SHA256SUMS" || fail "download failed: $release/SHA256SUMS"
want=$(awk '$2 == "sweeflow-x86_64.tar.gz" {print $1}' "$tmp/SHA256SUMS")
have=$(sha256sum "$tmp/sweeflow.tar.gz" | awk '{print $1}')
[ -n "$want" ] && [ "$want" = "$have" ] || fail "the download doesn't match the release's SHA256SUMS; try again"
tar -xzf "$tmp/sweeflow.tar.gz" -C "$tmp"
bin=$(ls -d "$tmp"/sweeflow-*/sweeflow)

# The libraries it loads, which every desktop has, though a minimal install may lack libva.
missing=$(ldd "$bin" 2>/dev/null | awk '/not found/ {print $1}' | tr '\n' ' ')
if [ -n "$missing" ]; then
    say "sweeflow: this system lacks: $missing" >&2
    if command -v apt-get >/dev/null; then say "  install them with: sudo apt install libva2 libva-drm2 libxkbcommon0 libfontconfig1" >&2
    elif command -v dnf >/dev/null; then say "  install them with: sudo dnf install libva libxkbcommon fontconfig" >&2
    elif command -v pacman >/dev/null; then say "  install them with: sudo pacman -S libva libxkbcommon fontconfig" >&2
    elif command -v zypper >/dev/null; then say "  install them with: sudo zypper install libva2 libva-drm2 libxkbcommon0 fontconfig" >&2
    fi
    exit 1
fi
# Not needed to start, but each one is a feature: the Mac's sound, and notifications.
command -v pw-cat >/dev/null || command -v pacat >/dev/null || say "    note: without pw-cat (PipeWire) or pacat (PulseAudio) you won't hear the Mac's sound"
command -v notify-send >/dev/null || say "    note: without notify-send (libnotify) Sweeflow can't show notifications"

say "==> installing"
"$bin" install
say "==> done: Sweeflow opens with your desktop from now on. If the Link a Mac window didn't open, run: sweeflow open"
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) say "    (~/.local/bin isn't on your PATH: run it as ~/.local/bin/sweeflow)" ;; esac
