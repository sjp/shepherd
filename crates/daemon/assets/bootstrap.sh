# Finds an agentbus of an exact version at this end and runs it, or says what
# this machine is so that one can be put here.
#
#   sh -s -- <version> <command...>
#
# The candidates are the places a person's own installation plausibly lives,
# tried before anything is written: finding a copy is cheaper and far less
# intrusive than pushing one over it. AGENTBUS_REMOTE_BINARY comes first so that
# somebody who knows where theirs is can say so.
#
# A candidate is accepted only when `--version` answers with exactly the wanted
# line. That comparison is the whole verification: a truncated copy, a copy for
# another architecture and a copy of a different release all fail it, and none of
# them is ever executed. It is also what makes provisioning idempotent — a far
# end that is already current costs this one round-trip and no writes.
#
# Exit 42 means "nothing here is right"; the line before it names the operating
# system and architecture, which is what decides the binary to send.

set -eu
ver="$1"
shift
for cand in ${AGENTBUS_REMOTE_BINARY:-} \
            "$(command -v agentbus 2>/dev/null || true)" \
            "$HOME/.local/bin/agentbus" \
            "$HOME/.linuxbrew/bin/agentbus" "/opt/homebrew/bin/agentbus" \
            "$HOME/.local/share/mise/shims/agentbus" \
            "$HOME/.nix-profile/bin/agentbus" \
            "/tmp/agentbus-$ver"; do
  [ -n "$cand" ] && [ -x "$cand" ] || continue
  [ "$("$cand" --version 2>/dev/null)" = "agentbus $ver" ] || continue
  exec "$cand" "$@"
done
echo "need=$(uname -s)/$(uname -m)"
exit 42
