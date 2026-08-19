# Says what an earlier installation of this program left on this machine.
#
#   sh -s
#
# Read before anything is written, because everything that decides whether a
# copy may be written is here: where an installation goes on this machine,
# whether the one path an installation ever writes to is occupied, and whether
# the record that says this program is what occupied it is there.
#
# Where those paths are is this machine's answer rather than the caller's, so
# the three of them are reported back and the caller composes nothing: the
# variables that move them are read here, by the shell that can see them.
#
# It writes nothing. A machine that has never seen this program answers with the
# paths and nothing else.

set -eu

printf 'bin=%s\n' "$bin"
printf 'partial=%s\n' "$partial"
printf 'record=%s\n' "$marker"
if [ -e "$bin" ]; then
  # Whatever is there is asked what it is, and one line of the answer is kept:
  # this is for telling somebody what is in the way, and a file that answers
  # with a paragraph is already not this program.
  printf 'binary=%s\n' "$("$bin" --version 2>/dev/null | head -n 1 || true)"
fi
if [ -f "$marker" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    printf 'marker %s\n' "$line"
  done < "$marker"
fi
