#!/bin/sh
# Builds the release binary and the Debian package: target/debian/sweeflow_<version>-1_amd64.deb, and
# a copy named sweeflow_amd64.deb, the name the website's Debian tab downloads.
#
# Run it on the Debian/Ubuntu release the package is for (a container is fine): the FFmpeg package
# names in Depends come from the libraries the binary is linked against, and they differ between
# releases (see [package.metadata.deb] in Cargo.toml). One .deb per release.
#
# Needs: cargo install --locked cargo-about --features cli, cargo install --locked cargo-deb, and
# dpkg-shlibdeps (the dpkg-dev package) for the Depends line. Without it cargo-deb can't fill in
# "$auto", so this stops rather than make a package that depends on nothing.
set -eu
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

command -v dpkg-shlibdeps >/dev/null || {
    echo "build-deb.sh: dpkg-shlibdeps not found (install dpkg-dev): the package's Depends can't be worked out without it" >&2
    exit 1
}

./licenses.sh
cargo build --release --locked
# --no-build: built just above.
cargo deb --no-build
deb=$(ls -t target/debian/sweeflow_*.deb | head -1)
cp "$deb" target/debian/sweeflow_amd64.deb
echo "built $deb (and target/debian/sweeflow_amd64.deb)"
