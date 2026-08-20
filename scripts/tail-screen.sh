#!/bin/sh
# Reads the end of a terminal recording and prints roughly what is on the screen
# now: escape sequences removed, redraws flattened, the last rows kept.
#
#   tail-screen.sh [-n ROWS] [-b BYTES] LOG
#
#     LOG             a recording made by script(1) with -f, so it is flushed as
#                     it is written and can be read while the session runs
#     -n, --rows      how many rows the screen is taken to be; default 30
#     -b, --bytes     how much of the end of the log to read; default 65536
#
# This is the stand-in for a multiplexer's screen grab, for a terminal that has
# no multiplexer in it. A recording is everything the agent ever drew, in order;
# a screen is only what survived being drawn over. Three of the ways a terminal
# discards output are replayed here, which is enough to read an agent's chrome:
#
#   - A clear-screen throws away everything before it, so only what was drawn
#     since the last one is considered.
#   - A carriage return means the next text overwrites the row rather than
#     following it, so of the several times a row was drawn only the last
#     survives. Without this a spinner redrawn once a second stacks up into a
#     wall of stale rows, and a rule that matches the spinner goes on matching
#     long after the agent has moved on to asking a question.
#   - Runs of blank rows collapse, as they do on a screen that scrolled.
#
# What is not replayed is cursor addressing: an agent that repaints one row in
# the middle of the screen by moving the cursor there leaves both copies here.
# Full-screen TUIs mostly clear and repaint, so this is survivable, but it is
# the reason a multiplexer's grab is worth preferring where there is one.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

rows=30
bytes=65536

usage() {
	sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2
	exit 2
}

while [ $# -gt 0 ]; do
	case $1 in
	-n | --rows)
		rows=${2?--rows needs a number}
		shift 2
		;;
	-b | --bytes)
		bytes=${2?--bytes needs a number}
		shift 2
		;;
	-h | --help) usage ;;
	-*)
		echo "tail-screen.sh: unknown option $1" >&2
		exit 2
		;;
	*) break ;;
	esac
done

[ $# -eq 1 ] || usage
log=$1

if [ ! -f "$log" ]; then
	echo "tail-screen.sh: no recording at $log" >&2
	exit 1
fi

# Everything before the last clear-screen is gone from the screen, so it is
# dropped before the escape sequences are stripped away and the evidence of the
# clear goes with them.
tail -c "$bytes" "$log" |
	perl -0777 -pe 'my $i = rindex($_, "\e[2J"); $_ = substr($_, $i) if $i >= 0;' |
	"$root/strip-escapes.sh" |
	perl -pe 's/\r$//; s/.*\r//;' |
	grep -v '^Script \(started\|done\) on ' |
	cat -s |
	tail -n "$rows"
