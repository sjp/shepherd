#!/bin/sh
# Runs the tests over the files `agentbus install` writes into coding agents:
# the invariants every one of them is held to, and the plugins, actually loaded
# and run by the interpreters their agents load them with.
#
# The plugins are the reason this exists as a script. They need `node` and
# `python3`, a machine may have neither, and the tests skip what it cannot run
# rather than failing — so what a developer needs is to be told what was skipped,
# which is what running them through here does. Pass --required where every case
# is expected to run, which is what continuous integration does: there, a missing
# interpreter is a machine that was set up wrong and not a case to leave out.
#
# Run from anywhere: it resolves paths relative to the repository root.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

required=0
for arg in "$@"; do
	case "$arg" in
	--required) required=1 ;;
	*)
		printf 'usage: %s [--required]\n' "$0" >&2
		exit 2
		;;
	esac
done

# The interpreters the plugins are written for. Reported either way, so that the
# output says what this machine was able to prove rather than only that it
# passed.
missing=
for interpreter in node python3; do
	if version=$("$interpreter" --version 2>&1); then
		printf '%s: %s\n' "$interpreter" "$version"
	else
		printf 'no %s here; the plugins written for it will be skipped\n' "$interpreter"
		missing="${missing:+$missing }$interpreter"
	fi
done

if [ -n "$missing" ] && [ "$required" -eq 1 ]; then
	printf 'FAIL: every case was expected to run, and this machine has no %s\n' "$missing" >&2
	exit 1
fi

# Not quiet on success, because a skipped case is only reported by the test that
# skipped it and a passing test says nothing by default.
exec cargo test -p agentbus-install --test assets --test asset_runtime -- --nocapture
