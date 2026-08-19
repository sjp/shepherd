# Takes away what this program put on this machine, and nothing else.
#
#   sh -s
#
# Two things go, in this order. The daemon serving this user is asked to stop,
# because it is the process holding the sockets and it is about to lose the
# binary it is running from; where its files are is resolved the same way the
# program itself resolves them — an explicit directory, then the session's
# runtime directory, then a per-user directory under /tmp.
#
# Then the binary, and only on the strength of the record left when it was
# installed. No record means this program never wrote here and nothing may be
# removed; a record naming another path means the same. A record whose version
# the file no longer answers with means somebody has replaced it since, and
# theirs is left where it is. The record itself goes either way: it is this
# program's own file, and after this it describes nothing.
#
# Every outcome is named on stdout so that whoever asked for this is told what
# happened rather than being left to infer it from silence.

set -eu
bin=$HOME/.local/bin/agentbus
marker=$HOME/.local/share/agentbus/installed

dir=${AGENTBUS_DIR:-}
if [ -z "$dir" ]; then
  if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    dir=$XDG_RUNTIME_DIR/agentbus
  else
    dir=/tmp/agentbus-$(id -u)
  fi
fi
lock=$dir/daemon.lock
if [ -f "$lock" ]; then
  pid=$(cat "$lock" 2>/dev/null || true)
  case ${pid:-nothing} in
    *[!0-9]*) ;;
    *) if kill -TERM "$pid" 2>/dev/null; then printf 'stopped=%s\n' "$pid"; fi ;;
  esac
fi

if [ ! -f "$marker" ]; then
  printf 'unrecorded=%s\n' "$bin"
  exit 0
fi
ver=
path=
while IFS= read -r line || [ -n "$line" ]; do
  case $line in
    version=*) ver=${line#version=} ;;
    path=*) path=${line#path=} ;;
  esac
done < "$marker"
rm -f "$marker"
printf 'forgot=%s\n' "$marker"

if [ "$path" != "$bin" ]; then
  printf 'elsewhere=%s\n' "$path"
  exit 0
fi
if [ ! -e "$bin" ]; then
  printf 'gone=%s\n' "$bin"
  exit 0
fi
said=$("$bin" --version 2>/dev/null || true)
if [ "$said" = "agentbus $ver" ]; then
  rm -f "$bin"
  printf 'removed=%s\n' "$bin"
else
  printf 'kept=%s\n' "$bin"
  printf 'said=%s\n' "$(printf '%s' "$said" | head -n 1)"
fi
