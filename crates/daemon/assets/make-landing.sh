# Makes sure this machine has the directory a borrowed copy goes in, and says
# where that is.
#
#   sh -s
#
# Run once per far end, on the way to writing a copy there and never on the way
# to finding one that is already there — the directory is made here, and a
# search that turns something up has to be able to say it changed nothing.
#
# Making it is not enough on its own, which is what the second line is for: a
# directory somebody else created first is one `mkdir -p` reports success for
# and one `chmod` quietly fails on, and a copy written into it would be a copy
# that user could replace between it being checked and it being run.

mkdir -m 700 -p "$landing"
mine "$landing" || {
  echo "$landing is not a directory this user keeps to itself" >&2
  exit 1
}
printf '%s\n' "$landing"
