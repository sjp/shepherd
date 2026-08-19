# Says what an earlier installation of this program left on this machine.
#
#   sh -s
#
# Read before anything is written, because everything that decides whether a
# copy may be written is here: where this machine's home directory really is,
# whether the one path an installation ever writes to is occupied, and whether
# the record that says this program is what occupied it is there.
#
# It writes nothing. A machine that has never seen this program answers with its
# home directory and nothing else.

set -eu
bin=$HOME/.local/bin/agentbus
marker=$HOME/.local/share/agentbus/installed

printf 'home=%s\n' "$HOME"
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
