# Assets

The hook and plugin templates that `agentbus install` writes into the coding
agents on a machine. Each file here is compiled into the binary with
`include_str!` by `src/assets.rs`, so that installing needs nothing on disk
beyond the binary itself — which matters because the same binary is copied onto
machines that have no checkout of this repository.

Templates name the `agentbus` binary by an absolute path substituted at install
time, never by a bare command: the directory a user installed the binary into is
not guaranteed to be on the `PATH` their coding agent runs hooks with.

One file, or one directory, per agent.
