# Finds an agentbus of an exact version at this end and runs it, or says what
# this machine is so that one can be put here.
#
#   sh -s -- <version> <command...>
#   sh -s -- <version>
#
# The candidates are the places a person's own installation plausibly lives,
# tried before anything is written: finding a copy is cheaper and far less
# intrusive than pushing one over it. AGENTBUS_REMOTE_BINARY comes first so that
# somebody who knows where theirs is can say so.
#
# The last candidate is the only one this program ever writes, and it is the
# only one that has to earn its place: a directory under /tmp is a directory
# anybody may create, so a copy found there is considered only when the
# directory turns out to belong to this user and to nobody else. Everything
# before it is a path somebody chose on purpose.
#
# A candidate is accepted only when `--version` answers with exactly the wanted
# line. That comparison is the whole verification: a truncated copy, a copy for
# another architecture and a copy of a different release all fail it, and none of
# them is ever executed. It is also what makes provisioning idempotent — a far
# end that is already current costs this one round-trip and no writes.
#
# Nothing here writes anything, including the directory the last candidate is
# in: a search that found what it was looking for has to be able to say it
# changed nothing at all.
#
# With no command to run, the search is the whole point: the accepted candidate
# is named on stdout as `found=<path>` and nothing is executed. That is what
# somebody asking "is a copy already here, and where" gets, as against somebody
# who wants one to take over the connection.
#
# Exit 42 means "nothing here is right"; the line before it names the operating
# system and architecture, which is what decides the binary to send. Whatever was
# passed over on the way there is named on stderr, one `other=<path><tab><what it
# said>` per candidate, so that a caller about to write can say which
# installations it is leaving alone. It goes to stderr because stdout belongs to
# whatever takes the connection over.

set -eu
ver="$1"
shift

borrowed=
if mine "$landing"; then
  borrowed=$landing/agentbus-$ver
fi

for cand in ${AGENTBUS_REMOTE_BINARY:-} \
            "$(command -v agentbus 2>/dev/null || true)" \
            "${XDG_BIN_HOME:+$XDG_BIN_HOME/agentbus}" \
            "${HOME:+$HOME/.local/bin/agentbus}" \
            "${HOME:+$HOME/.linuxbrew/bin/agentbus}" "/opt/homebrew/bin/agentbus" \
            "${HOME:+$HOME/.local/share/mise/shims/agentbus}" \
            "${HOME:+$HOME/.nix-profile/bin/agentbus}" \
            "$borrowed"; do
  [ -n "$cand" ] && [ -x "$cand" ] || continue
  said=$("$cand" --version 2>/dev/null || true)
  if [ "$said" = "agentbus $ver" ]; then
    [ "$#" -gt 0 ] || { echo "found=$cand"; exit 0; }
    exec "$cand" "$@"
  fi
  printf 'other=%s\t%s\n' "$cand" "$(printf '%s' "$said" | head -n 1)" >&2
done
echo "need=$(uname -s)/$(uname -m)"
exit 42
