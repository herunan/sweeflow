#!/bin/sh
# Builds everything a Linux release ships, into dist/linux/ at the repo root:
#   sweeflow-VERSION-x86_64.tar.gz   the binary, launcher, icon and licences (install.sh and the Arch
#                                    package unpack it); also as sweeflow-x86_64.tar.gz
#   sweeflow_VERSION-1_amd64.deb     Debian, Ubuntu, Mint, Pop!_OS... (also as sweeflow_amd64.deb)
#   sweeflow-VERSION-1.x86_64.rpm    Fedora, openSUSE... (also as sweeflow.x86_64.rpm)
#   sweeflow-VERSION-REL-x86_64.pkg.tar.zst (+ .sig)   Arch, CachyOS, Omarchy... (also as
#                                    sweeflow-x86_64.pkg.tar.zst, for pacman -U)
#   sweeflow.db, sweeflow.files (+ .sig)   Sweeflow's pacman repo: the release's download folder is its
#                                    Server; signed with the Sweeflow release key, whose public half is
#   sweeflow.asc                     this (pacman-key --add)
#   install.sh                       curl ... | sh for any distro
#   ffmpeg-*.tar.xz, FFMPEG-CONFIGURE.txt   the source of the FFmpeg inside, as its licence (LGPL) asks
#   SHA256SUMS
# One binary for all of them, built in env.sh's Ubuntu 22.04 with FFmpeg linked in (ffmpeg.sh), so it runs
# on any distro with glibc 2.35 or later and doesn't care which FFmpeg the distro has.
#
# Usage: linux/release/build.sh (the Mac app is its own download: ../../release-mac.sh)
# PKGREL=2 for a second Arch package of the same version (pacman only updates to a higher one).
# The release key's GnuPG home: ~/.local/share/sweeflow-release/gnupg (SWEEFLOW_GNUPG elsewhere).
set -eu
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' linux/Cargo.toml | head -1)
out=dist/linux
rm -rf "$out" && mkdir -p "$out"

linux/release/env.sh '
set -eu
export CARGO_TARGET_DIR=/src/linux/target/portable SWEEFLOW_PORTABLE=1
cd linux
cargo build --release --locked --features portable
./licenses.sh
cp target/THIRD-PARTY-LICENSES.html target/portable/THIRD-PARTY-LICENSES.html
# cargo-deb: target/release/ in the assets means $CARGO_TARGET_DIR/release/; Depends from dpkg-shlibdeps,
# against Ubuntu 22.04, so the lowest versions anything needs.
cargo deb --no-build --features portable --output /src/dist/linux/
'
bin=linux/target/portable/release/sweeflow
lic=linux/target/portable/THIRD-PARTY-LICENSES.html

# Nothing of FFmpeg's may be loaded from the system.
if objdump -p "$bin" | grep -E 'NEEDED.*lib(av|sw)'; then echo "build.sh: the binary links a system FFmpeg" >&2; exit 1; fi
echo "==> needs: $(objdump -p "$bin" | awk '/NEEDED/{print $2}' | tr '\n' ' ')"
echo "==> newest glibc symbol: $(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -t. -k2,2n -k3,3n -u | tail -1)"

# The tarball.
stage=$(mktemp -d)
d="$stage/sweeflow-$version"
mkdir -p "$d"
cp "$bin" "$d/sweeflow"
cp "$lic" LICENSE linux/packaging/sweeflow.desktop "$d/"
cp assets/sweeflow-app.svg "$d/sweeflow.svg"
tar -C "$stage" -czf "$out/sweeflow-$version-x86_64.tar.gz" "sweeflow-$version"
cp "$out/sweeflow-$version-x86_64.tar.gz" "$out/sweeflow-x86_64.tar.gz"

# The RPM, from the same tarball (rpmbuild in the build environment, which sees the repo as /src).
rpm=linux/target/portable/rpm
rm -rf "$rpm" && mkdir -p "$rpm/SOURCES"
cp "$out/sweeflow-$version-x86_64.tar.gz" "$rpm/SOURCES/"
linux/release/env.sh "rpmbuild -bb --define '_topdir /src/$rpm' --define 'version $version' linux/release/sweeflow.spec"
cp "$rpm/RPMS/x86_64/sweeflow-$version-1.x86_64.rpm" "$out/"
cp "$out/sweeflow-$version-1.x86_64.rpm" "$out/sweeflow.x86_64.rpm"
rm -rf "$stage"

cp "$out"/sweeflow_"$version"-1_amd64.deb "$out/sweeflow_amd64.deb"

# The Arch package, from the same tarball (makepkg here: this is an Arch machine), signed, and the
# pacman repo around it.
arch=linux/target/portable/arch
rm -rf "$arch" && mkdir -p "$arch"
cp linux/release/arch/PKGBUILD linux/release/arch/sweeflow.install "$out/sweeflow-$version-x86_64.tar.gz" "$arch/"
pkgrel=${PKGREL:-1}
sed -i "s/^pkgver=.*/pkgver=$version/; s/^pkgrel=.*/pkgrel=$pkgrel/" "$arch/PKGBUILD"
export GNUPGHOME="${SWEEFLOW_GNUPG:-$HOME/.local/share/sweeflow-release/gnupg}"
key=$(gpg --list-secret-keys --with-colons 2>/dev/null | awk -F: '/^fpr/{print $10; exit}')
[ -n "$key" ] || { echo "no Sweeflow release key in $GNUPGHOME" >&2; exit 1; }
(cd "$arch" && PKGDEST="$PWD" GPGKEY="$key" makepkg -f --sign --nodeps --noconfirm >/dev/null)
pkg="sweeflow-$version-$pkgrel-x86_64.pkg.tar.zst"
cp "$arch/$pkg" "$arch/$pkg.sig" "$out/"
cp "$arch/$pkg" "$out/sweeflow-x86_64.pkg.tar.zst"
cp "$arch/$pkg.sig" "$out/sweeflow-x86_64.pkg.tar.zst.sig"
(cd "$out" && repo-add --quiet --sign --key "$key" sweeflow.db.tar.zst "$pkg")
# A release's downloads can't be links: what pacman asks for (sweeflow.db…) as files of their own.
for f in sweeflow.db sweeflow.db.sig sweeflow.files sweeflow.files.sig; do
    t=$(readlink "$out/$f") && cp --remove-destination "$out/$t" "$out/$f"
done
gpg --armor --export "$key" > "$out/sweeflow.asc"
cp linux/release/install.sh "$out/"
cp ~/.cache/sweeflow-build/jammy/opt/ffmpeg/share/source/ffmpeg-*.tar.xz "$out/"
cp ~/.cache/sweeflow-build/jammy/opt/ffmpeg/share/source/CONFIGURE.txt "$out/FFMPEG-CONFIGURE.txt"
(cd "$out" && sha256sum -- * > SHA256SUMS)
ls -la "$out"
