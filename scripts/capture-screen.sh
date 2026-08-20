#!/bin/sh
# Takes the screen a coding agent is drawing right now and files it as a test
# case: escape sequences removed, the terminal's title recorded beside it, and a
# blank verdict left for the person who was watching to fill in.
#
#   capture-screen.sh [-p PANE] [-t TITLE] [-d DIR] AGENT CASE
#   capture-screen.sh --stdin [-t TITLE] [-d DIR] AGENT CASE
#   capture-screen.sh --log FILE [-n ROWS] [-t TITLE] [-d DIR] AGENT CASE
#
#     AGENT           the manifest id whose rules answer for it: claude, codex,
#                     opencode
#     CASE            what the screen is a case of: working, idle, blocked-bash,
#                     transcript
#     -p, --pane      the tmux pane to read; default is the active one
#     --stdin         read the screen from standard input instead of from tmux.
#                     With a terminal on the other end this waits for you to
#                     paste the screen and press ctrl-D, which is how to capture
#                     from a terminal that has no multiplexer in it
#     --log FILE      read the end of a script(1) recording instead, via
#                     tail-screen.sh
#     -n, --rows      how many rows to take from that recording; default 30
#     -t, --title     the title the terminal was showing, when the capture
#                     cannot be asked for it
#     -d, --dir       where to write; default is the tree inside the detect
#                     crate that the replay test reads
#
# Two or three files come out: CASE.txt is the screen, CASE.expected is the
# verdict line, and CASE.title appears only when there was a title to record.
#
# What this cannot do is decide what the screen meant. It writes CASE.expected
# with the verdict commented out and the current reading noted, so that filling
# it in is a deliberate act by the person who saw the session rather than a
# transcription of whatever the rules already believe. A fixture that records
# today's answer proves nothing tomorrow.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

pane=
title=
stdin=no
log=
rows=30
dir=$root/crates/detect/tests/fixtures/screens

usage() {
	sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2
	exit 2
}

while [ $# -gt 0 ]; do
	case $1 in
	-p | --pane)
		pane=${2?--pane needs a pane}
		shift 2
		;;
	-t | --title)
		title=${2?--title needs a title}
		shift 2
		;;
	-d | --dir)
		dir=${2?--dir needs a directory}
		shift 2
		;;
	--stdin)
		stdin=yes
		shift
		;;
	--log)
		log=${2?--log needs a file}
		shift 2
		;;
	-n | --rows)
		rows=${2?--rows needs a number}
		shift 2
		;;
	-h | --help) usage ;;
	-*)
		echo "capture-screen.sh: unknown option $1" >&2
		exit 2
		;;
	*) break ;;
	esac
done

[ $# -eq 2 ] || usage
agent=$1
case=$2

# The case name becomes a file name and a test label, so keep it to what reads
# the same in both.
case $agent in
*[!a-z0-9-]* | '')
	echo "capture-screen.sh: $agent is not a manifest id" >&2
	exit 2
	;;
esac
case $case in
*[!a-z0-9-]* | '')
	echo "capture-screen.sh: $case is not a case name" >&2
	exit 2
	;;
esac

out=$dir/$agent
mkdir -p "$out"

# Stripping escape sequences is one job with one answer, and it lives next door
# so that a live loop reaches the same one.
strip_escapes() {
	"$root/scripts/strip-escapes.sh"
}

if [ -n "$log" ]; then
	"$root/scripts/tail-screen.sh" -n "$rows" "$log" >"$out/$case.txt"
elif [ "$stdin" = yes ]; then
	# A person pasting into a terminal has no way to know this is waiting.
	if [ -t 0 ]; then
		echo "capture-screen.sh: paste the screen, then press ctrl-D" >&2
	fi
	strip_escapes >"$out/$case.txt"
else
	command -v tmux >/dev/null 2>&1 || {
		echo "capture-screen.sh: no tmux; capture the screen yourself and pass --stdin" >&2
		exit 1
	}
	# -p to standard output, -e to keep the escapes so this script's own
	# stripping is the only one in play, and no -S: the rules read the rows the
	# agent is drawing on now, not scrollback.
	tmux capture-pane -p -e ${pane:+-t "$pane"} | strip_escapes >"$out/$case.txt"

	if [ -z "$title" ]; then
		title=$(tmux display-message -p ${pane:+-t "$pane"} '#{pane_title}' 2>/dev/null || true)
	fi
fi

# A pane title that is only the host or the shell's idea of the command is not
# the agent's title, but this script cannot tell the difference; it records what
# it found and leaves the judgement to the reader.
if [ -n "$title" ]; then
	printf '%s\n' "$title" >"$out/$case.title"
else
	rm -f "$out/$case.title"
fi

# What the rules make of it as it stands, shown but not adopted: a capture whose
# .expected was copied from the current answer would pass this test for ever,
# including on the day the answer becomes wrong.
reading=$(
	"$root/target/debug/agentbus" detect --agent "$agent" \
		${title:+--osc-title "$title"} --json <"$out/$case.txt" 2>/dev/null || true
)

{
	echo "# $agent, $case"
	echo "#"
	echo "# Replace the line below with what you saw: one of working, idle,"
	echo "# blocked, unknown, then 'visible' if the state's chrome was live on"
	echo "# the screen and 'skip' if the screen was showing the past."
	if [ -n "$reading" ]; then
		echo "#"
		echo "# The rules currently read it as: $reading"
	fi
	echo "TODO"
} >"$out/$case.expected"

echo "wrote $out/$case.txt"
[ -f "$out/$case.title" ] && echo "wrote $out/$case.title"
echo "wrote $out/$case.expected  <- fill in its last line"
