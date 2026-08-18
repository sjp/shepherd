#!/bin/sh
# Mechanical checks over the crates in this workspace; exits non-zero on the first
# failure. Run from anywhere: it resolves paths relative to the repository root.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

CRATES="crates/protocol crates/daemon crates/install crates/cli"
status=0

fail() {
	printf 'FAIL: %s\n' "$1" >&2
	status=1
}

# 1. No crate in the workspace may depend on a crate under crates/shepherd.
printf 'checking manifests for a dependency on crates/shepherd\n'
for crate in $CRATES; do
	manifest="$crate/Cargo.toml"
	[ -f "$manifest" ] || { fail "$manifest is missing"; continue; }
	if hits=$(grep -inE 'path[[:space:]]*=[[:space:]]*"[^"]*shepherd' "$manifest"); then
		fail "$manifest declares a path dependency on shepherd:"
		printf '%s\n' "$hits" >&2
	fi
	if hits=$(grep -inE '^[[:space:]]*[A-Za-z0-9_-]*shepherd[A-Za-z0-9_-]*[[:space:]]*[.=]' "$manifest"); then
		fail "$manifest declares a dependency named after shepherd:"
		printf '%s\n' "$hits" >&2
	fi
done

# 2. The word must not appear anywhere in the crates themselves. The repository
#    README, the workspace manifest and docs/ are deliberately not scanned.
printf 'checking %s for the word "shepherd"\n' "$CRATES"
if hits=$(grep -rinI shepherd $CRATES); then
	fail 'the word "shepherd" appears in a crate:'
	printf '%s\n' "$hits" >&2
fi

# 3. Nothing under crates/ may point at the design document or the backlog.
printf 'checking crates/ for references to PLAN.md or issues/\n'
if hits=$(grep -rinIF -e 'PLAN.md' -e 'issues/' crates); then
	fail 'a crate references the design document or the backlog:'
	printf '%s\n' "$hits" >&2
fi

if [ "$status" -eq 0 ]; then
	printf 'boundary check passed\n'
fi

exit "$status"
