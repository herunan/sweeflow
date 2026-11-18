#!/bin/sh
# Runs a command in the release build environment: an Ubuntu 22.04 root filesystem (glibc 2.35), so the
# binary built there runs on any distro from 2022 on (Ubuntu 22.04+, Debian 12+, Mint 21+, Fedora 36+,
# Arch). It needs no root and no containers: the filesystem is entered through a user namespace
# (unshare --map-root-user --map-auto, which needs newuidmap and an entry in /etc/subuid).
#
# The first run downloads Ubuntu's base image (~30 MB) and installs the build tools, Rust (rustup) and a
# small static FFmpeg (ffmpeg.sh) into it, under ~/.cache/sweeflow-build/jammy. The repository is seen
# inside as /src. Usage: env.sh COMMAND [ARGS...]  (run as a shell command line)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
cache="${SWEEFLOW_BUILD_CACHE:-$HOME/.cache/sweeflow-build}"
root="$cache/jammy"
base=ubuntu-base-22.04.5-base-amd64.tar.gz

mkdir -p "$cache"
if [ ! -x "$root/usr/bin/apt-get" ]; then
    [ -f "$cache/$base" ] || curl -fL -o "$cache/$base" "https://cdimage.ubuntu.com/ubuntu-base/releases/22.04/release/$base"
    mkdir -p "$root"
    unshare --map-root-user --map-auto tar -xzf "$cache/$base" -C "$root"
fi
cp /etc/resolv.conf "$root/etc/resolv.conf"
mkdir -p "$root/src"

# Inside: the repo at /src, /dev and /proc from the host, a plain environment.
run() {
    unshare --map-root-user --map-auto --mount --pid --fork sh -c '
        root=$1; repo=$2; shift 2
        mount --rbind /dev "$root/dev" && mount -t proc proc "$root/proc" && mount --bind "$repo" "$root/src"
        exec chroot "$root" /usr/bin/env -i HOME=/root TERM=dumb DEBIAN_FRONTEND=noninteractive \
            PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
            PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig /bin/sh -c "cd /src && $*"' sh "$root" "$repo" "$@"
}

if [ ! -f "$root/.sweeflow-ready" ]; then
    run 'apt-get update -q && apt-get install -y -q --no-install-recommends build-essential nasm pkg-config curl \
        ca-certificates git xz-utils clang libclang-dev libva-dev libdrm-dev libxkbcommon-dev libfontconfig-dev \
        libfreetype-dev libwayland-dev libx11-dev libegl-dev libgl-dev libdbus-1-dev dpkg-dev rpm file python3'
    run 'command -v cargo >/dev/null || curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal'
    run 'cargo install --locked cargo-about --features cli && cargo install --locked cargo-deb'
    run 'linux/release/ffmpeg.sh'
    touch "$root/.sweeflow-ready"
fi

run "$@"
