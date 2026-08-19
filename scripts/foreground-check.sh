#!/bin/sh
# Reads one bus and prints, for one correlation, what the process table says is
# running in front of its terminal and what its agent sessions are doing. Both
# halves come from the machine-readable output of `agentbus foreground --json`
# and `agentbus status --json`, so what is printed here is what a program
# reading the bus would see, laid out for a person watching a terminal.
#
#   foreground-check.sh [-w] [-p] [CORRELATION]
#
#     CORRELATION  the value a shell carries as AGENTBUS_PANE; defaults to this
#                  shell's own, and to every correlation when there is none
#     -w, --watch  print again every second until interrupted
#     -p, --pid    print only the observed pids, one per line, and nothing else
#
# AGENTBUS_DIR selects the bus, exactly as it does for agentbus itself.
# AGENTBUS_BIN names the binary to ask; otherwise the first of agentbus on PATH,
# target/release/agentbus and target/debug/agentbus that exists is used.
#
# The three answers `agentbus foreground` distinguishes are kept distinct here
# too: observations, "nothing is running in that correlation", and "there is no
# daemon or it is not watching a process table" are three different lines, and
# this script exits with the same code that told them apart — 0, 1 and 2.

set -eu

usage() {
	awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

watch=0
pids_only=0
correlation=${AGENTBUS_PANE:-}

while [ $# -gt 0 ]; do
	case $1 in
	-w | --watch) watch=1 ;;
	-p | --pid) pids_only=1 ;;
	-h | --help)
		usage
		exit 0
		;;
	-*)
		printf 'foreground-check: unknown option %s\n' "$1" >&2
		usage >&2
		exit 2
		;;
	*) correlation=$1 ;;
	esac
	shift
done

command -v jq >/dev/null 2>&1 || {
	printf 'foreground-check: jq is needed to read the JSON output\n' >&2
	exit 2
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ -n "${AGENTBUS_BIN:-}" ]; then
	bin=$AGENTBUS_BIN
elif command -v agentbus >/dev/null 2>&1; then
	bin=$(command -v agentbus)
elif [ -x "$root/target/release/agentbus" ]; then
	bin=$root/target/release/agentbus
elif [ -x "$root/target/debug/agentbus" ]; then
	bin=$root/target/debug/agentbus
else
	printf 'foreground-check: no agentbus binary; build one or set AGENTBUS_BIN\n' >&2
	exit 2
fi

# Seconds since an ISO 8601 timestamp, for a jq program. Fractional seconds are
# dropped rather than parsed: the age is printed to the second either way.
age='def age: (sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601) as $t
	| (now - $t) | floor | tostring + "s ago";'

# The observations, as newline-delimited JSON. The exit code carries an answer of
# its own, so it is captured rather than allowed to end the script.
observations() {
	if [ -n "$correlation" ]; then
		"$bin" foreground --json --correlation "$correlation"
	else
		"$bin" foreground --json
	fi
}

# The code the last foreground answer came back with, which is this script's own.
answer=0

report_foreground() {
	out=$(observations 2>&1) && answer=0 || answer=$?
	case $answer in
	0)
		printf '%s\n' "$out" | jq -r "$age"'
			"  pid \(.pid)  \(.state // "no state")  \(.process)  [\(.cmdline)]  since \(.since | age)"
		'
		;;
	1) printf '  nothing is running in %s\n' "$correlation" ;;
	*) printf '  no answer: %s\n' "$out" ;;
	esac
}

report_sessions() {
	out=$("$bin" status --json 2>&1) || {
		printf '  no answer: %s\n' "$out"
		return 0
	}
	printf '%s\n' "$out" | jq -r --arg correlation "$correlation" "$age"'
		[.sessions[] | select($correlation == "" or .correlation == $correlation)] as $wanted
		| if ($wanted | length) == 0 then "  no sessions"
		  else $wanted[]
			| "  \(.agent) \(.session)  \(.status)  by \(.source)  since \(.since | age)"
			  + "  correlation \(.correlation // "none")"
		  end
	'
}

report_pids() {
	out=$(observations 2>&1) && answer=0 || answer=$?
	[ "$answer" -eq 0 ] || return "$answer"
	printf '%s\n' "$out" | jq -r '.pid'
}

report() {
	printf 'bus %s  via %s\n' "${AGENTBUS_DIR:-the session runtime directory}" "$bin"
	printf 'correlation %s\n' "${correlation:-<every one>}"
	printf 'foreground\n'
	report_foreground
	printf 'sessions\n'
	report_sessions
}

if [ "$pids_only" -eq 1 ]; then
	report_pids || exit "$answer"
	exit 0
fi

if [ "$watch" -eq 0 ]; then
	report
	exit "$answer"
fi

while :; do
	printf '\033[H\033[2J'
	report
	sleep 1
done
