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

# The release build (release/build.sh) carries FFmpeg, built by release/ffmpeg.sh under the LGPL: its
# licence, version, configuration and where its source is go on the page too. Other builds load the
# system's FFmpeg, and the marker just goes.
ff=/opt/ffmpeg/share/source
if [ "${SWEEFLOW_PORTABLE:-}" = 1 ]; then
    [ -f "$ff/COPYING.LGPLv2.1" ] || { echo "licenses.sh: no FFmpeg licence in $ff (release/ffmpeg.sh)" >&2; exit 1; }
    {
        echo '<h2>FFmpeg</h2>'
        echo "<p>This build of Sweeflow contains FFmpeg $(pkg-config --modversion libavcodec | sed 's/^/(libavcodec /;s/$/)/'),"
        echo 'built from <a href="https://ffmpeg.org/">ffmpeg.org</a>'"'"'s '"$(ls "$ff" | grep -o 'ffmpeg-[0-9.]*[0-9]')"' source and licensed under the'
        echo 'GNU Lesser General Public License 2.1 or later. Its source and the configuration used are published with each'
        echo 'Sweeflow release (ffmpeg-*.tar.xz and FFMPEG-CONFIGURE.txt, at https://github.com/herunan/sweeflow/releases),'
        echo 'and Sweeflow'"'"'s own source is open, so it can be rebuilt with a modified FFmpeg. Configuration:</p><pre>'
        sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' "$ff/CONFIGURE.txt"
        echo '</pre><pre>'
        sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' "$ff/COPYING.LGPLv2.1"
        echo '</pre>'
    } > target/ffmpeg-licence.html
    sed -i -e '/<!--FFMPEG-->/{r target/ffmpeg-licence.html' -e 'd}' "$out"
    rm target/ffmpeg-licence.html
else
    sed -i '/<!--FFMPEG-->/d' "$out"
fi
echo "wrote linux/$out"
