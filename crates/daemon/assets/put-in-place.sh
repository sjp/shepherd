# Makes the copy that has just been sent the one this machine runs.
#
#   sh -s -- <version>
#
# The copy arrives beside the name it is to have, never on it. This is what
# moves it, and the order is the whole of the safety: the file is made runnable,
# asked what it is, and only then renamed. A rename is atomic, so the path a hook
# may exec at any moment is either the copy that was there before or the whole
# new one, and never half of either — and a copy that arrived truncated or built
# for another machine is removed here rather than ever being at that name.
#
# The record written afterwards is what makes taking it away again safe: it says
# this program put this version at this path, and nothing else may be removed on
# the strength of it.

set -eu
ver="$1"

mkdir -p "${bin%/*}"
chmod 755 "$partial"
said=$("$partial" --version 2>/dev/null || true)
if [ "$said" != "agentbus $ver" ]; then
  rm -f "$partial"
  # A copy that arrived whole and still will not answer is usually one of three
  # things, and the last is the one nobody thinks of: a truncated transfer, a
  # binary for another machine, or a filesystem mounted `noexec`, which refuses
  # to run a file it is perfectly happy to have been given.
  printf 'the copy that arrived at %s answers "%s" rather than "agentbus %s"; a \
truncated transfer, a binary for another machine and a filesystem mounted \
noexec all look like this\n' \
    "${bin%/*}" "$(printf '%s' "$said" | head -n 1)" "$ver" >&2
  exit 1
fi

mv -f "$partial" "$bin"
said=$("$bin" --version 2>/dev/null || true)
if [ "$said" != "agentbus $ver" ]; then
  printf '%s answers "%s" rather than "agentbus %s"\n' \
    "$bin" "$(printf '%s' "$said" | head -n 1)" "$ver" >&2
  exit 1
fi

mkdir -p "${marker%/*}"
printf 'version=%s\npath=%s\n' "$ver" "$bin" > "$marker"
printf 'installed=%s\n' "$bin"
