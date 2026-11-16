#!/bin/sh
# Writes target/THIRD-PARTY-LICENSES.html: the licences of every Rust crate Sweeflow is built from
# (about.toml says which licences are accepted, about.hbs lays out the page). The packages install it
# (build-deb.sh). HTML rather than text because cargo-about groups the crates under each licence, and a
# page with an index is easier to read than one long file; any browser opens it.
#
# Needs: cargo install --locked cargo-about --features cli
set -eu
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p target
out=target/THIRD-PARTY-LICENSES.html

# --locked: the licences of exactly the versions in Cargo.lock.
cargo about generate --locked about.hbs -o "$out"

# Slint is offered under the GPL, its Royalty-free licence or a commercial one. Sweeflow uses the
# Royalty-free one (about.toml), but the slint crates ship that licence as a file cargo-about can't
# pick up, so its text is added from the slint crate itself: the version the binary is built with.
slint_dir=$(cargo metadata --locked --format-version 1 | python3 -c '
import json, os, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == "slint":
        print(os.path.dirname(p["manifest_path"]))
')
text="$slint_dir/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md"
[ -f "$text" ] || { echo "licenses.sh: no Slint licence text at $text" >&2; exit 1; }
{
    echo '<h2>Slint Royalty-free licence</h2>'
    echo '<p>Slint (the slint and i-slint-* crates above) is used under this licence.</p><pre>'
    sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' "$text"
    echo '</pre>'
} > target/slint-licence.html
sed -i -e '/<!--SLINT-->/{r target/slint-licence.html' -e 'd}' "$out"
rm target/slint-licence.html
echo "wrote linux/$out"
