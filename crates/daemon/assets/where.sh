# Where an installation of this program lives on this machine.
#
# Prepended to every script that reads or writes one, so that the three paths
# have one definition between them rather than one per script — and so that the
# machine the paths are on is the machine that decides them, which is the only
# end that can read the variables below.
#
# AGENTBUS_REMOTE_BINARY names the whole path when it is set. It is already the
# first thing looked at when searching for a copy, and already what somebody is
# told to set when the ordinary path turns out to be occupied; letting it decide
# the write as well is what makes it impossible for the search and the
# installation to disagree about where the copy is.
#
# Failing that, `$XDG_BIN_HOME` and `$XDG_DATA_HOME` if this machine sets them,
# and otherwise the per-user directories those default to.

set -eu

if [ -n "${XDG_BIN_HOME:-}" ]; then
  bindir=$XDG_BIN_HOME
elif [ -n "${HOME:-}" ]; then
  bindir=$HOME/.local/bin
else
  bindir=
fi
bin=${AGENTBUS_REMOTE_BINARY:-${bindir:+$bindir/agentbus}}

if [ -n "${XDG_DATA_HOME:-}" ]; then
  datadir=$XDG_DATA_HOME
elif [ -n "${HOME:-}" ]; then
  datadir=$HOME/.local/share
else
  datadir=
fi
marker=${datadir:+$datadir/agentbus/installed}

# Every path resolves before any of them is insisted on, so a machine with no
# usable home directory still works when it has been told where to put the copy.
# Only a machine that has been told nothing is stuck, and naming the variables
# that would answer is more use than naming what is missing.
if [ -z "$bin" ] || [ -z "$marker" ]; then
  echo "there is no usable HOME here; set AGENTBUS_REMOTE_BINARY to say where an \
agentbus should go, and XDG_DATA_HOME to say where the record of it should go — \
or attach to this machine rather than installing on it, which needs neither" >&2
  exit 1
fi
partial=${bin%/*}/.agentbus.tmp
