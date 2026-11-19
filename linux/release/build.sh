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
#   InRelease, Release(.gpg), Packages(.gz)   Sweeflow's apt repo (deb ... releases/latest/download ./)
#   rpm/ (repodata, sweeflow.repo)   Sweeflow's dnf repo index, for sweeflow.app/rpm/ (a release can't hold a
#                                    folder): it points at this release's .rpm
#   Sweeflow.dmg                     the Mac app, if ../../release-mac.sh made it (dist/Sweeflow.dmg)
#   latest.json                      what's new and where to get it: the update notice of both apps
#   SHA256SUMS(.sig)                 signed, so an update is checked before it's installed
#   install.sh                       curl ... | sh for any distro
#   ffmpeg-*.tar.xz, FFMPEG-CONFIGURE.txt   the source of the FFmpeg inside, as its licence (LGPL) asks
# One binary for all of them, built in env.sh's Ubuntu 22.04 with FFmpeg linked in (ffmpeg.sh), so it runs
# on any distro with glibc 2.35 or later and doesn't care which FFmpeg the distro has.
#
# Usage: linux/release/build.sh (the Mac app is its own download: ../../release-mac.sh)
# PKGREL=2 for a second Arch package of the same version (pacman only updates to a higher one).
# The release key's GnuPG home: ~/.local/share/sweeflow-release/gnupg (SWEEFLOW_GNUPG elsewhere).
set -eu
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' linux/Cargo.toml | head -1)
# The build number: both apps compare it with the release's (update.rs, Updater.swift) to see a rebuild of
# one version as newer.
build=${BUILD:-$(git rev-list --count HEAD)}
# Where the release's files are downloaded from (GitHub; the local stand-in for tests): what the update
# notice, the apt repo and the dnf repo point at.
release_url=${RELEASE_URL:-https://github.com/herunan/sweeflow/releases/download/v$version}
out=dist/linux
rm -rf "$out" && mkdir -p "$out"

linux/release/env.sh '
set -eu
export CARGO_TARGET_DIR=/src/linux/target/portable SWEEFLOW_PORTABLE=1 SWEEFLOW_BUILD='"$build"'
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

# The apt repo, flat in the same folder: Packages for the versioned .deb only (the other is a copy), a
# Release listing it, signed.
linux/release/env.sh '
set -eu
cd /src/dist/linux
apt-ftparchive packages . 2>/dev/null | awk -v RS= -v ORS="\n\n" "!/Filename: .\/sweeflow_amd64.deb/" > Packages
gzip -9kn Packages
apt-ftparchive -o APT::FTPArchive::Release::Origin=Sweeflow -o APT::FTPArchive::Release::Label=Sweeflow \
    -o APT::FTPArchive::Release::Suite=stable -o APT::FTPArchive::Release::Architectures=amd64 release . > /tmp/Release
mv /tmp/Release Release
'
gpg --batch --yes --clearsign --digest-algo SHA512 -o "$out/InRelease" "$out/Release"
gpg --batch --yes --detach-sign --digest-algo SHA512 -o "$out/Release.gpg" "$out/Release"

# The dnf repo's index (repodata/), for sweeflow.app/rpm/: the packages stay in the release, so their
# addresses in it are the release's.
rpmrepo=linux/target/portable/rpmrepo
rm -rf "$rpmrepo" && mkdir -p "$rpmrepo"
cp "$out/sweeflow-$version-1.x86_64.rpm" "$rpmrepo/"
linux/release/env.sh "createrepo_c --quiet --baseurl '$release_url/' /src/$rpmrepo"
mkdir -p "$out/rpm"
cp -r "$rpmrepo/repodata" "$out/rpm/"
gpg --batch --yes --armor --detach-sign -o "$out/rpm/repodata/repomd.xml.asc" "$out/rpm/repodata/repomd.xml"
cp "$out/sweeflow.asc" "$out/rpm/sweeflow.asc"
cat > "$out/rpm/sweeflow.repo" <<REPO
[sweeflow]
name=Sweeflow
baseurl=${RPM_REPO_URL:-https://sweeflow.app/rpm}
enabled=1
repo_gpgcheck=1
gpgcheck=0
gpgkey=${RPM_REPO_URL:-https://sweeflow.app/rpm}/sweeflow.asc
REPO

# The Mac app, made first by release-mac.sh (with its build number), goes up with the rest.
mac_json=null
if [ -f dist/Sweeflow.dmg ]; then
    cp dist/Sweeflow.dmg "$out/Sweeflow.dmg"
    mac_build=$(cat dist/Sweeflow.build 2>/dev/null || echo "$build")
    mac_json=$(printf '{"version": "%s", "build": %s, "url": "%s/Sweeflow.dmg", "sha256": "%s"}' \
        "$version" "$mac_build" "$release_url" "$(sha256sum "$out/Sweeflow.dmg" | cut -d" " -f1)")
fi
# The update notice: this release's notes (CHANGELOG.md's section for it) and where each app gets it.
python3 - "$version" "$build" "$release_url" "$mac_json" "$out" <<'PY'
import json, re, sys, hashlib, datetime
version, build, url, mac, out = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
log = open("CHANGELOG.md").read()
m = re.search(r"^## " + re.escape(version) + r"\s*\n(.*?)(?=^## |\Z)", log, re.S | re.M)
notes = m.group(1).strip() if m else ""
tar = f"sweeflow-{version}-x86_64.tar.gz"
sha = hashlib.sha256(open(f"{out}/{tar}", "rb").read()).hexdigest()
feed = {
    "version": version,
    "date": datetime.date.today().isoformat(),
    "notes": notes,
    "page": f"https://github.com/herunan/sweeflow/releases/tag/v{version}",
    "mac": json.loads(mac),
    "linux": {"version": version, "build": build, "url": f"{url}/{tar}", "sha256": sha},
}
open(f"{out}/latest.json", "w").write(json.dumps(feed, indent=2) + "\n")
PY
cp linux/release/install.sh "$out/"
cp ~/.cache/sweeflow-build/jammy/opt/ffmpeg/share/source/ffmpeg-*.tar.xz "$out/"
cp ~/.cache/sweeflow-build/jammy/opt/ffmpeg/share/source/CONFIGURE.txt "$out/FFMPEG-CONFIGURE.txt"
(cd "$out" && find . -maxdepth 1 -type f ! -name SHA256SUMS ! -name SHA256SUMS.sig -printf '%P\n' | sort | xargs sha256sum -- > SHA256SUMS)
gpg --batch --yes --detach-sign -o "$out/SHA256SUMS.sig" "$out/SHA256SUMS"
ls -la "$out"
