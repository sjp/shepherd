#!/bin/sh
# Puts the window inside the only container macOS accepts as an application: a
# .app directory with an Info.plist at the top of it. Run as a bare executable
# the process has no identity of its own — the menu bar keeps naming whatever
# terminal started it, the Dock has nowhere to put it, and there is no About
# panel. None of that is something the program can ask for; it is what the
# bundle tells the system before the program starts.
#
#   bundle-macos.sh [-o DIR] [-i ICON] [TRIPLE]
#
#     TRIPLE       a macOS target triple; default is this machine's
#     -o, --out    where the bundle is assembled; default dist/
#     -i, --icon   an .icns to use as it is, or a square .png to build one
#                  from; default is the placeholder this script draws
#
# The result is DIR/Shepherd.app, ad-hoc signed so that the machine it was
# built on will launch it without a word. It is not notarised and is not meant
# to be given to anybody else.
#
# Two numbers in the plist are read rather than written down. The version is the
# workspace's, the same one the binary answers --version with. The oldest system
# the bundle admits to is taken off the built binary itself, because that is the
# floor the compiler actually chose and a plist that disagreed with it would
# either turn away machines that work or admit machines that cannot run it.
#
# This only runs on a Mac. The toolkit the window is drawn with compiles its
# shaders with Xcode's own tools on the machine doing the building, so there is
# no cross-build of it from anywhere else, and every tool used below to assemble
# and sign the bundle is the system's.

set -eu

usage() {
	awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

outdir=dist
icon=
triple=

while [ $# -gt 0 ]; do
	case $1 in
	-o | --out)
		[ $# -ge 2 ] || {
			printf 'bundle-macos: %s needs a directory\n' "$1" >&2
			exit 2
		}
		outdir=$2
		shift
		;;
	-i | --icon)
		[ $# -ge 2 ] || {
			printf 'bundle-macos: %s needs a file\n' "$1" >&2
			exit 2
		}
		icon=$2
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	-*)
		printf 'bundle-macos: unknown option %s\n' "$1" >&2
		usage >&2
		exit 2
		;;
	*) triple=$1 ;;
	esac
	shift
done

# Every path below eventually reaches one of the system's own tools, and those
# read their arguments their own way; a path beginning with a dash is spelt
# relative to here instead, where nothing can mistake it for an option.
case $outdir in -*) outdir=./$outdir ;; esac
case $icon in -*) icon=./$icon ;; esac

[ "$(uname -s)" = Darwin ] || {
	printf 'bundle-macos: this builds and signs a macOS application; it needs a Mac\n' >&2
	exit 1
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
. scripts/release-lib.sh

version=$(workspace_version)
[ -n "$version" ] || {
	printf 'bundle-macos: no version in Cargo.toml\n' >&2
	exit 1
}

[ -n "$triple" ] || triple=$(rustc -vV | sed -n 's/^host: //p')
case $triple in
*-apple-darwin) ;;
*)
	printf 'bundle-macos: %s is not a macOS target\n' "$triple" >&2
	exit 2
	;;
esac

# The target's standard library, if this machine has rustup and has not got it
# already. A machine without rustup is expected to have arranged its own.
if command -v rustup >/dev/null 2>&1; then
	rustup target list --installed | grep -qx -- "$triple" || rustup target add -- "$triple"
fi

# --locked because a build somebody is going to run as an application comes from
# the dependency graph this repository recorded, not from whatever resolves today.
cargo build --release --locked --target "$triple" --package shepherd

built=target/$triple/release/shepherd
[ -x "$built" ] || {
	printf 'bundle-macos: %s was not produced\n' "$built" >&2
	exit 1
}

# The deployment target the compiler stamped into the binary, which is the
# oldest system it can be loaded on. Recent toolchains record it in
# LC_BUILD_VERSION as "minos"; if that cannot be read, say so and fall back to
# the oldest release either Apple Silicon target has ever been built for, which
# is conservative in the direction that turns nobody away.
minimum=$(otool -l "$built" 2>/dev/null | awk '$1 == "minos" { print $2; exit }' || true)
if [ -z "$minimum" ]; then
	minimum=11.0
	printf 'no deployment target found in %s: saying %s\n' "$built" "$minimum"
fi

app=$outdir/Shepherd.app
contents=$app/Contents
resources=$contents/Resources

rm -rf -- "$app"
mkdir -p -- "$contents/MacOS" "$resources"
cp -- "$built" "$contents/MacOS/shepherd"
# No end-of-options marker after the mode, unlike everywhere else here: this
# machine's chmod stops reading options at the first operand and would take the
# marker for a file that does not exist. There is nothing for it to protect
# against anyway — the name is this script's own.
chmod +x "$contents/MacOS/shepherd"

# Draws one square icon. Pure standard library, so that a placeholder costs the
# repository no committed binary and no tool that is not already here.
draw_png() {
	python3 - "$1" "$2" <<'PY'
import math
import struct
import sys
import zlib

size = int(sys.argv[1])
out = sys.argv[2]

GROUND = (0x23, 0x2A, 0x31)
MARK = (0xE8, 0xED, 0xF2)


def rounded_square(px, py, half, radius):
    qx = abs(px) - half + radius
    qy = abs(py) - half + radius
    return (
        math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - radius
    )


def ring(px, py, radius, width):
    return abs(math.hypot(px, py) - radius) - width / 2.0


def coverage(distance):
    # A distance in pixels read as how much of the pixel the shape covers. One
    # pixel of edge is all the antialiasing a flat shape of this size needs.
    return min(max(0.5 - distance, 0.0), 1.0)


half = size * 0.5
box = size * 0.42
corner = size * 0.2237
radius = size * 0.20
width = size * 0.075

rows = []
for y in range(size):
    py = y + 0.5 - half
    row = bytearray()
    for x in range(size):
        px = x + 0.5 - half
        ground = coverage(rounded_square(px, py, box, corner))
        if ground <= 0.0:
            row += b"\0\0\0\0"
            continue
        # The mark sits inside the ground and never outside it, so the pixel's
        # opacity is the ground's and its colour is how far along the mark is.
        mark = coverage(ring(px, py, radius, width)) * ground
        t = min(mark / ground, 1.0)
        for g, m in zip(GROUND, MARK):
            row.append(round(g + (m - g) * t))
        row.append(round(ground * 255.0))
    rows.append(bytes(row))


def chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


raw = b"".join(b"\0" + row for row in rows)
png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    + chunk(b"IDAT", zlib.compress(raw, 9))
    + chunk(b"IEND", b"")
)
with open(out, "wb") as handle:
    handle.write(png)
PY
}

# Turns a square png into the .icns the bundle wants, by way of the directory of
# sizes the system's own converter reads. Every size is drawn or scaled once and
# copied to the two names that share it.
make_icns() {
	source=$1
	target=$2
	iconset=$work/Shepherd.iconset
	rm -rf -- "$iconset"
	mkdir -p -- "$iconset"
	for spec in \
		16:icon_16x16 32:icon_16x16@2x \
		32:icon_32x32 64:icon_32x32@2x \
		128:icon_128x128 256:icon_128x128@2x \
		256:icon_256x256 512:icon_256x256@2x \
		512:icon_512x512 1024:icon_512x512@2x; do
		size=${spec%%:*}
		if [ ! -f "$work/$size.png" ]; then
			if [ -n "$source" ]; then
				sips -s format png -z "$size" "$size" "$source" \
					--out "$work/$size.png" >/dev/null
			else
				draw_png "$size" "$work/$size.png"
			fi
		fi
		cp -- "$work/$size.png" "$iconset/${spec#*:}.png"
	done
	iconutil -c icns -o "$target" "$iconset"
}

work=$(mktemp -d "${TMPDIR:-/tmp}/shepherd-bundle.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

case $icon in
'')
	make_icns '' "$resources/Shepherd.icns"
	printf 'icon: a placeholder, drawn here; pass --icon for a real one\n'
	;;
*.icns)
	cp -- "$icon" "$resources/Shepherd.icns"
	printf 'icon: %s\n' "$icon"
	;;
*)
	make_icns "$icon" "$resources/Shepherd.icns"
	printf 'icon: built from %s\n' "$icon"
	;;
esac

# Anything else the application needs to carry with it belongs beside the icon,
# in Resources, and is found at runtime relative to the executable.
cat >"$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIdentifier</key>
	<string>io.github.sjp.shepherd</string>
	<key>CFBundleName</key>
	<string>Shepherd</string>
	<key>CFBundleDisplayName</key>
	<string>Shepherd</string>
	<key>CFBundleExecutable</key>
	<string>shepherd</string>
	<key>CFBundleIconFile</key>
	<string>Shepherd</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>$minimum</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
PLIST

plutil -lint "$contents/Info.plist"

# Ad-hoc: signed with no identity at all, which is enough for the machine that
# built it to run it without argument. Anything leaving this machine would need
# a real certificate and a trip through notarisation, and neither is this
# script's business.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

printf '%s  Shepherd %s, %s, macOS %s or newer\n' "$app" "$version" "$triple" "$minimum"
