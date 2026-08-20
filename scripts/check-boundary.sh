#!/bin/sh
# Mechanical checks over the crates in this workspace; exits non-zero on the first
# failure. Run from anywhere: it resolves paths relative to the repository root.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# Every crate in the workspace except crates/shepherd, derived at runtime so that a
# crate added later is scanned without anyone remembering to update this list.
CRATES=
for dir in crates/*/; do
	crate=${dir%/}
	if [ ! -d "$crate" ]; then
		continue
	fi
	if [ "$crate" = "crates/shepherd" ]; then
		continue
	fi
	CRATES="${CRATES:+$CRATES }$crate"
done

status=0

fail() {
	printf 'FAIL: %s\n' "$1" >&2
	status=1
}

if [ -z "$CRATES" ]; then
	fail 'no crates found under crates/'
	exit "$status"
fi

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

# 4. The upstream project whose manifest data this workspace vendors lives in an
#    uncommitted directory in the working tree. The shipped crates must read as a
#    standalone artifact, so its name may not appear in source, comments, docs,
#    tests or assets. Legal attribution is the one exception: Apache-2.0 requires
#    the vendored data's origin to be named, and NOTICE/LICENSE files are where it
#    goes.
printf 'checking %s for the vendored reference name\n' "$CRATES"
if hits=$(grep -rinI --exclude='NOTICE*' --exclude='LICENSE*' herdr $CRATES); then
	fail 'the vendored reference is named in a crate:'
	printf '%s\n' "$hits" >&2
fi

# 5. That directory is expected in the working tree and must never be committed.
#    Outside a git checkout there is nothing to check, so say nothing.
if git rev-parse --git-dir >/dev/null 2>&1; then
	printf 'checking that the vendored reference is untracked\n'
	tracked=$(git ls-files herdr 2>/dev/null || true)
	if [ -n "$tracked" ]; then
		fail 'the vendored reference directory is tracked by git:'
		printf '%s\n' "$tracked" >&2
	fi
fi

if [ "$status" -eq 0 ]; then
	printf 'boundary check passed\n'
fi

exit "$status"
