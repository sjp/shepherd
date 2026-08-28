#!/bin/sh
# Prints one screenful of everything a terminal has to draw correctly, so that a
# person can look at a terminal and see whether it does.
#
#   sample-screen.sh [SECTION...]
#
#     ruler      a column ruler, for checking that everything below lines up
#     width      wide characters and combining marks against that ruler
#     colour     the sixteen named colours, the 256-colour palette, 24-bit
#     attributes bold, dim, italic, underline, inverse, strikeout, hidden
#     cursor     each cursor shape in turn, a few seconds apart
#
# With no section it prints all of them but `cursor`, which takes a while.
#
# Written for a person's eye rather than for a test: what "correct" looks like
# is described beside each section, because the mistakes worth catching here —
# a wide character overlapping its neighbour, a mark landing beside its letter
# instead of over it — are ones you see rather than ones you assert.

set -eu

# How wide the ruler and the rows measured against it are.
COLUMNS=60

# How long each cursor shape is shown for.
CURSOR_PAUSE=4

esc() {
	printf '\033%s' "$1"
}

heading() {
	printf '\n\033[1m%s\033[0m — %s\n' "$1" "$2"
}

# A ruler of COLUMNS columns: a digit every column, a bar every tenth.
ruler() {
	heading 'ruler' 'every row below starts at column 1 and is 60 columns wide'
	i=0
	while [ "$i" -lt "$COLUMNS" ]; do
		if [ $((i % 10)) -eq 0 ]; then
			printf '|'
		else
			printf '%d' $((i % 10))
		fi
		i=$((i + 1))
	done
	printf '\n'
}

# Characters that are not one column of one codepoint.
width() {
	heading 'width' 'each row is 60 columns; the last character of each ends at the ruler'

	# Thirty wide characters, which should occupy exactly sixty columns.
	printf 'CJK       '
	i=0
	while [ "$i" -lt 25 ]; do
		printf '\346\274\242'	# U+6F22, a full-width character
		i=$((i + 1))
	done
	printf '\n'

	printf 'emoji     '
	i=0
	while [ "$i" -lt 25 ]; do
		printf '\360\237\230\200'	# U+1F600, also two columns wide
		i=$((i + 1))
	done
	printf '\n'

	# A base character and a combining mark are one column between them: this
	# row and the next must end in the same place.
	printf 'combining '
	i=0
	while [ "$i" -lt 50 ]; do
		printf 'e\314\201'	# e + U+0301, an acute accent over it
		i=$((i + 1))
	done
	printf '\n'

	printf 'plain     '
	i=0
	while [ "$i" -lt 50 ]; do
		printf 'e'
		i=$((i + 1))
	done
	printf '\n'

	printf 'composed  '
	i=0
	while [ "$i" -lt 25 ]; do
		printf '\303\251 '	# U+00E9, the same letter as one codepoint
		i=$((i + 1))
	done
	printf '\n'

	printf 'stacked   a\314\201\314\200\314\202\314\203 one column with four marks on it\n'
	printf 'mixed     a\346\274\242b\360\237\230\200c narrow, wide, narrow, wide, narrow\n'
}

# Colours, from the eight every terminal has to twenty-four bits of them.
colour() {
	heading 'colour' 'each block is its own colour, and the words on it are readable'

	printf 'named     '
	i=0
	while [ "$i" -lt 8 ]; do
		printf '\033[3%dm%d\033[0m ' "$i" "$i"
		i=$((i + 1))
	done
	i=0
	while [ "$i" -lt 8 ]; do
		printf '\033[9%dm%d\033[0m ' "$i" $((i + 8))
		i=$((i + 1))
	done
	printf '\n'

	printf 'on them   '
	i=0
	while [ "$i" -lt 8 ]; do
		printf '\033[4%d;30m %d \033[0m' "$i" "$i"
		i=$((i + 1))
	done
	printf '\n'

	printf 'the cube  '
	i=16
	while [ "$i" -lt 52 ]; do
		printf '\033[48;5;%dm \033[0m' "$i"
		i=$((i + 1))
	done
	printf '\n'

	printf 'the greys '
	i=232
	while [ "$i" -lt 256 ]; do
		printf '\033[48;5;%dm \033[0m' "$i"
		i=$((i + 1))
	done
	printf '\n'

	printf 'true      '
	i=0
	while [ "$i" -lt 48 ]; do
		printf '\033[48;2;%d;%d;%dm \033[0m' $((i * 5)) $((255 - i * 5)) 128
		i=$((i + 1))
	done
	printf '\n'
}

# Everything a cell can be beyond its colour.
attributes() {
	heading 'attributes' 'the hidden row is blank; every other row shows what it says'
	printf 'bold      \033[1mbold\033[0m and \033[1;31mbold red\033[0m\n'
	printf 'dim       \033[2mdim\033[0m beside \033[0mplain\033[0m\n'
	printf 'italic    \033[3mitalic\033[0m\n'
	printf 'underline \033[4munderlined\033[0m \033[21mdouble\033[0m \033[4:3mcurly\033[0m\n'
	printf 'inverse   \033[7minverse video\033[0m\n'
	printf 'strikeout \033[9mstruck through\033[0m\n'
	printf 'hidden    \033[8mthis should not be readable\033[0m|\n'
	printf 'together  \033[1;4;7mbold underlined inverse\033[0m\n'
}

# Each shape the cursor can be, one at a time.
#
# On a timer rather than on a key press: a terminal that cannot be typed into
# yet still has a cursor, and this is how to look at it.
cursor() {
	heading 'cursor' "the cursor changes shape every ${CURSOR_PAUSE}s; the last one is the default"
	for shape in '2 block' '4 underline' '6 bar' '1 blinking block' '0 the default'; do
		style=${shape%% *}
		printf '\ncursor is %s ' "${shape#* }"
		esc "[$style q"
		sleep "$CURSOR_PAUSE"
	done
	printf '\n'
}

if [ "$#" -eq 0 ]; then
	set -- ruler width colour attributes
fi

for section in "$@"; do
	case "$section" in
	ruler) ruler ;;
	width) width ;;
	colour | color) colour ;;
	attributes) attributes ;;
	cursor) cursor ;;
	*)
		printf 'sample-screen.sh: no section called %s\n' "$section" >&2
		exit 2
		;;
	esac
done
