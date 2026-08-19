#!/bin/sh
# Describes a directory of built assets as the manifest a machine reads when it
# has to fetch the binary rather than be handed one: which target triples exist,
# where each is, how long it is and what it hashes to.
#
#   make-manifest.sh [-o FILE] [-V VERSION] DIR BASE_URL
#
#     DIR             a directory holding assets named agentbus-<version>-<triple>
#     BASE_URL        where those assets will be reachable; each asset's own
#                     name is appended to it, so pass the directory part only
#     -o, --out FILE  write here instead of to standard output
#     -V, --version   the version to describe; default is the workspace's
#
# The shape is fixed, because the thing that reads it is somewhere else and
# older copies of it stay in the world:
#
#   {
#     "v": 1,
#     "name": "agentbus",
#     "version": "0.1.0",
#     "assets": [
#       {
#         "triple": "x86_64-unknown-linux-musl",
#         "url": "https://example.invalid/v0.1.0/agentbus-0.1.0-x86_64-unknown-linux-musl",
#         "sha256": "<64 lowercase hex digits>",
#         "size": 4459304
#       }
#     ]
#   }
#
# "v" is the version of this shape and is 1. "sha256" and "size" describe the
# bytes exactly as they will be downloaded. Assets are listed in triple order so
# that regenerating a manifest for the same directory produces the same file.

set -eu

usage() {
	awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

out=-
version=
dir=
base=

while [ $# -gt 0 ]; do
	case $1 in
	-o | --out)
		[ $# -ge 2 ] || {
			printf 'make-manifest: %s needs a file\n' "$1" >&2
			exit 2
		}
		out=$2
		shift
		;;
	-V | --version)
		[ $# -ge 2 ] || {
			printf 'make-manifest: %s needs a version\n' "$1" >&2
			exit 2
		}
		version=$2
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	-*)
		printf 'make-manifest: unknown option %s\n' "$1" >&2
		usage >&2
		exit 2
		;;
	*)
		if [ -z "$dir" ]; then
			dir=$1
		elif [ -z "$base" ]; then
			base=$1
		else
			printf 'make-manifest: unexpected argument %s\n' "$1" >&2
			exit 2
		fi
		;;
	esac
	shift
done

[ -n "$dir" ] && [ -n "$base" ] || {
	printf 'make-manifest: a directory and a base URL are needed\n' >&2
	usage >&2
	exit 2
}
[ -d "$dir" ] || {
	printf 'make-manifest: %s is not a directory\n' "$dir" >&2
	exit 1
}
command -v jq >/dev/null 2>&1 || {
	printf 'make-manifest: jq is needed to write the JSON\n' >&2
	exit 1
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$root/scripts/release-lib.sh"

if [ -z "$version" ]; then
	version=$(cd "$root" && workspace_version)
fi
[ -n "$version" ] || {
	printf 'make-manifest: no version to describe\n' >&2
	exit 1
}

base=${base%/}

entries=$(
	for path in "$dir"/agentbus-"$version"-*; do
		[ -f "$path" ] || continue
		name=${path##*/}
		jq -n \
			--arg triple "${name#agentbus-$version-}" \
			--arg url "$base/$name" \
			--arg sha256 "$(sha256_of "$path")" \
			--argjson size "$(wc -c <"$path" | tr -d ' ')" \
			'{triple: $triple, url: $url, sha256: $sha256, size: $size}'
	done
)

[ -n "$entries" ] || {
	printf 'make-manifest: no assets named agentbus-%s-<triple> in %s\n' "$version" "$dir" >&2
	exit 1
}

manifest=$(printf '%s\n' "$entries" | jq -s \
	--arg version "$version" \
	'{v: 1, name: "agentbus", version: $version, assets: sort_by(.triple)}')

if [ "$out" = - ]; then
	printf '%s\n' "$manifest"
else
	printf '%s\n' "$manifest" >"$out"
fi
