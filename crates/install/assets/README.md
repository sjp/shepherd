# Assets

The files `agentbus install` writes into the coding agents on a machine. Each
file here is compiled into the binary with `include_str!` by `src/assets.rs`, so
that installing needs nothing on disk beyond the binary itself — which matters
because the same binary is copied onto machines that have no checkout of this
repository.

One directory per agent. Where an agent needs a different file on each kind of
machine, both sit in that directory side by side; which one is written is decided
when an installation is planned, not when this program is compiled.

Files name the `agentbus` binary by an absolute path substituted at install time,
never by a bare command: the directory a user installed the binary into is not
guaranteed to be on the `PATH` their coding agent runs hooks with.

Two marks go in the opening comment of every file written whole. The first line
says this program wrote it, so that a file of the user's own that happens to
share a name is never touched. A line below it says which generation of that
agent's hooks the file is, as `AGENTBUS_HOOK_VERSION=<number>`, so that a machine
can be asked whether what it carries is what this build writes. What the files
themselves must and must not do is written down in `src/assets.rs`.
