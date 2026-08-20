#!/bin/sh
# Turns a terminal's raw bytes into the text that was on it: escape sequences
# removed, nothing else touched.
#
#   strip-escapes.sh < raw > text
#
# Stripping belongs to whatever captured the bytes, because that is what knows
# how they were encoded; what the detection rules read is text. This is that
# step on its own, so a capture and a live loop can share one answer to it
# rather than each carrying its own regular expressions.

set -eu

# The two shapes a terminal emits: CSI and friends, which end in a byte from @
# to ~, and the string sequences (OSC, DCS, APC) that run until a BEL or a
# string terminator. Bracketed-paste markers are CSI sequences too, so pasting a
# screen into a capture is covered by the same pass.
exec sed -E \
	-e 's/\x1b[]P^_][^\x07\x1b]*(\x07|\x1b\\)?//g' \
	-e 's/\x1b[[][0-9;?]*[ -\/]*[@-~]//g' \
	-e 's/\x1b[()][A-Za-z0-9]//g' \
	-e 's/\x1b[@-Z\\-_]//g' \
	-e 's/\x0f//g'
