#!/bin/sh
# Builds one release binary the way a release builds it, so that the command
# that produces a published asset is also the command somebody runs on their own
# machine when they need one.
#
#   build-release.sh [-o DIR] TRIPLE
#
#     TRIPLE      a Rust target triple, e.g. x86_64-unknown-linux-musl
#     -o, --out   where the finished asset goes; default dist/
#
# The asset is named agentbus-<version>-<triple>, with no extension, where
# <version> is the workspace version. Its sha256 is printed beside it.
#
# Two things are checked before an asset is called finished. A musl target is
# linked statically — no interpreter and no shared library it would have to find
# on the far machine, which is the whole reason those targets exist. And the
# binary answers --version with exactly the line this workspace's version
# implies, byte for byte, because that answer is how a provisioned copy is
# recognised and a wrong one makes every remote binary look stale.
#
# The second of those needs the binary to run here. It runs natively when the
# target's architecture and operating system are this machine's, and otherwise
# through AGENTBUS_RUN_WRAPPER when one is given — `qemu-aarch64-static`, say.
# With neither, the check is skipped and says which it was.

set -eu

usage() {
	awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

outdir=dist
triple=

while [ $# -gt 0 ]; do
	case $1 in
	-o | --out)
		[ $# -ge 2 ] || {
			printf 'build-release: %s needs a directory\n' "$1" >&2
			exit 2
		}
		outdir=$2
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	-*)
		printf 'build-release: unknown option %s\n' "$1" >&2
		usage >&2
		exit 2
		;;
	*) triple=$1 ;;
	esac
	shift
done

[ -n "$triple" ] || {
	printf 'build-release: no target triple\n' >&2
	usage >&2
	exit 2
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
. scripts/release-lib.sh

version=$(workspace_version)
[ -n "$version" ] || {
	printf 'build-release: no version in Cargo.toml\n' >&2
	exit 1
}

# The target's standard library, if this machine has rustup and has not got it
# already. A machine without rustup is expected to have arranged its own.
if command -v rustup >/dev/null 2>&1; then
	rustup target list --installed | grep -qx -- "$triple" || rustup target add -- "$triple"
fi

# --locked because a release is built from the dependency graph this repository
# recorded, not from whatever resolves today.
cargo build --release --locked --target "$triple" --package agentbus-cli

built=target/$triple/release/agentbus
[ -x "$built" ] || {
	printf 'build-release: %s was not produced\n' "$built" >&2
	exit 1
}

asset=$outdir/agentbus-$version-$triple
mkdir -p -- "$outdir"
cp -- "$built" "$asset"
chmod +x -- "$asset"

# Statically linked means two absences: no PT_INTERP, so no dynamic loader is
# named, and no DT_NEEDED, so no shared library is wanted. A static PIE keeps a
# dynamic section for its own relocations, so the section being there proves
# nothing either way and only the entries count.
check_static() {
	if command -v readelf >/dev/null 2>&1; then
		if readelf -lW -- "$1" | grep -q INTERP; then
			printf 'build-release: %s names a dynamic loader\n' "$1" >&2
			exit 1
		fi
		if readelf -dW -- "$1" 2>/dev/null | grep -q NEEDED; then
			printf 'build-release: %s needs a shared library\n' "$1" >&2
			exit 1
		fi
		printf 'statically linked: no interpreter, no shared library\n'
	elif command -v file >/dev/null 2>&1; then
		file -- "$1" | grep -q 'statically linked' || {
			printf 'build-release: %s is not statically linked\n' "$1" >&2
			exit 1
		}
		printf 'statically linked (as file(1) reports it)\n'
	else
		printf 'not checked for static linking: neither readelf nor file is here\n'
	fi
}

case $triple in
*-linux-musl) check_static "$asset" ;;
*) printf 'not a musl target: static linking is not expected\n' ;;
esac

# Whether this machine can run what was just built. Architecture and operating
# system have to match; the libc does not, since a static musl binary runs on a
# glibc machine of the same architecture.
host=$(rustc -vV | sed -n 's/^host: //p')
os_of() {
	case $1 in
	*-linux-*) printf 'linux\n' ;;
	*-apple-darwin) printf 'darwin\n' ;;
	*) printf 'other\n' ;;
	esac
}
native=no
if [ "${triple%%-*}" = "${host%%-*}" ] && [ "$(os_of "$triple")" = "$(os_of "$host")" ]; then
	native=yes
fi

wrapper=${AGENTBUS_RUN_WRAPPER:-}
if [ -n "$wrapper" ]; then
	how="through $wrapper"
elif [ "$native" = yes ]; then
	how="natively"
else
	how=
fi

if [ -n "$how" ]; then
	# A template, because the bare command is not portable to every mktemp.
	expected=$(mktemp "${TMPDIR:-/tmp}/agentbus-version.XXXXXX")
	trap 'rm -f -- "$expected"' EXIT
	printf 'agentbus %s\n' "$version" >"$expected"
	# Unquoted on purpose: a wrapper may be a command with arguments.
	if $wrapper "$asset" --version | cmp -s - "$expected"; then
		printf '%s --version says "agentbus %s", %s\n' "$asset" "$version" "$how"
	else
		printf 'build-release: %s did not answer --version with "agentbus %s" %s\n' \
			"$asset" "$version" "$how" >&2
		exit 1
	fi
else
	printf 'not checked for --version: %s cannot run here; set AGENTBUS_RUN_WRAPPER\n' "$triple"
fi

printf '%s  %s\n' "$(sha256_of "$asset")" "$asset"
