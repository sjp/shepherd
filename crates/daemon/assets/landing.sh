# Where a copy of this program goes on a machine that is only borrowing it, and
# how to tell whether that place is this user's.
#
# Prepended to every script that has to name that directory, so that there is
# one answer rather than one per script. It writes nothing: the directory is
# created by whatever is about to put a file in it, because a search that found
# what it was looking for must be able to say it changed nothing.
#
# The rule is the one this program's own runtime directory follows — the
# session's runtime directory, then a per-user directory under /tmp — and it is
# per-user for the reason every other path here is: two people on one machine
# have no business sharing a file that one of them is about to execute.
# AGENTBUS_DIR is deliberately not consulted. That variable moves the sockets,
# which a caller may point anywhere for a test or a second bus, and a binary
# that moved with them would be a surprise nobody asked for.

set -eu

if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  landing=$XDG_RUNTIME_DIR/agentbus
else
  landing=/tmp/agentbus-$(id -u)
fi

# Whether $1 is a directory belonging to this user that nobody else may write.
#
# Resolving the path is not enough on its own. /tmp is world-writable, so a
# directory whose name is predictable is a directory somebody else may have
# created first — and `mkdir -p` succeeds against one, `chmod` on one fails
# without saying so. Anything under a directory that fails this is neither
# written nor run.
#
# The owner is read out of `ls -ldn`, whose numeric third field is the uid,
# rather than asked for with `find -user`: that takes a name or a uid and
# prefers the name, so on a machine with a user called `1000` it answers a
# different question than the one being asked.
mine() {
  [ -d "$1" ] || return 1
  set -- $(ls -ldn "$1" 2>/dev/null)
  case ${1:-} in
    d???------) ;;
    *) return 1 ;;
  esac
  [ "${3:-}" = "$(id -u)" ]
}
