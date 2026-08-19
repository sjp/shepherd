# Sourced, not run: the two or three answers that the release scripts and the
# release workflow must all give identically.
#
#   . scripts/release-lib.sh
#
# Every function here expects the repository root as the working directory.

# The one version number in this workspace. Every crate takes it from
# [workspace.package], and it is what the binary prints for --version, what
# names a release asset, and what a release tag has to agree with.
workspace_version() {
	sed -n '/^\[workspace\.package\]/,/^\[.*\]$/ s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml |
		sed -n 1p
}

# The sha256 of a file, as 64 lowercase hex digits and nothing else. Linux
# spells the tool one way and macOS the other.
sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -- "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 -- "$1" | cut -d' ' -f1
	else
		printf 'release-lib: no sha256sum and no shasum\n' >&2
		return 1
	fi
}
