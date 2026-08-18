# Decision log

Technology decisions for this repository, newest section last. Each entry records
what was chosen and why, so that work done in one crate does not quietly diverge
from work done in another. Change an entry by adding a dated replacement below it
rather than editing history in place.

## 2026-08-18 — initial set

### Language edition and toolchain

**Rust edition 2024; minimum supported toolchain 1.85.** 1.85 is the first release
that can build edition 2024, and the workspace declares `rust-version = "1.85"` so
an older toolchain fails with a clear message instead of a parse error. Development
currently happens on 1.97. There is no `rust-toolchain.toml`: pinning an exact
toolchain would force a download on every machine that builds this, for no benefit
while the code stays on stable.

### Command-line parsing

**`clap`, derive API.** It gives the usage output, exit codes and `--version`
handling that the CLI would otherwise hand-roll, and the derive API keeps the
command surface readable as the number of subcommands grows.

### JSON

**`serde` with `serde_json`.** The event protocol is JSON on the wire and JSON in
the agents' hook payloads, and `serde` is the only serious option. The protocol
crate takes these two dependencies and no others.

### Error handling

**`thiserror` for library error types, `anyhow` only in the binary.** Library
crates return concrete enums so callers can match on the failure and decide
whether it is recoverable; the binary is where errors stop being handled and start
being reported, and that is the only place a type-erased error is appropriate.

### Logging and diagnostics

**`tracing`, written to stderr and never to stdout.** Stdout carries
machine-readable output — the newline-delimited event stream and the version line
— so a stray log message on it would corrupt a consumer's input. Verbosity is
controlled by an environment filter, off by default.

### Daemon concurrency

**The daemon uses `tokio`.** It has to hold many things open at once: two listening
sockets, one fan-out per subscriber, a child process per attached remote endpoint
whose output must be read continuously and restarted with backoff when it dies,
and a periodic scan of the process table. Expressing supervision and reconnection
as tasks is considerably less error-prone than the equivalent thread and channel
plumbing, and the runtime is confined to the daemon.

### The emit path never touches the runtime

**Emitting an event uses `std` blocking sockets with explicit read and write
timeouts, and must never initialise an async runtime.** This code runs inside a
coding agent's hook, on every tool call, and its whole budget is about 100 ms
including process start; it has to be up in low single-digit milliseconds. Starting
a runtime costs more than the work itself, and a thread pool spun up in a process
that lives for a few milliseconds is a liability. The timeouts are mandatory: a
hook must fail fast and silently rather than block the agent that invoked it.

### The `--version` contract

**`agentbus --version` prints exactly `agentbus <semver>` followed by one newline,
and exits 0.** Provisioning this binary onto another machine involves copying it
there and asking it what it is; that answer is compared byte-for-byte against what
was expected. No build hash, no date, no banner. An integration test pins the bytes.

### Workspace layout and naming

**One workspace, one shared version.** Package names are prefixed —
`agentbus-protocol`, `agentbus-daemon`, `agentbus-install`, `agentbus-cli` — while
the directories under `crates/` keep the short names. The single binary produced by
`agentbus-cli` is called `agentbus`. All crates share `[workspace.package] version`,
and that is the version `--version` reports, so there is only ever one number to
bump.

**`protocol` has no I/O and no dependencies beyond `serde` and `serde_json`.** It is
the part every other component agrees on and the part that must be exhaustively
unit-testable, which only stays true if it cannot touch a socket, a file or a clock.

### Formatting and lints

**`rustfmt` defaults, pinned by the presence of `rustfmt.toml` with the 2024 style
edition. Clippy's default lint set is warned on at workspace level and promoted to
an error in continuous integration** (`cargo clippy --workspace --all-targets --
-D warnings`). Starting from the default set rather than `pedantic` keeps a clippy
warning meaningful; individual lints can be raised later if they earn it.

### Cargo.lock

**Committed.** This workspace ships a binary, and reproducing a reported version
means building from the same dependency graph it was built from.

## 2026-08-18 — daemon core

### No datetime dependency anywhere

**The daemon converts the system clock to a protocol timestamp itself.** The
protocol's timestamps are one fixed shape — RFC 3339, UTC, milliseconds — chosen
so that the crate defining them needs no calendar. Producing one is then about
fifteen lines of civil-calendar arithmetic, exhaustively testable, against a
datetime library's build time, API churn and time-zone database. If a later
requirement needs real calendar handling — local time, zones, parsing what a user
typed — that is the point to reconsider, and only in the crate that needs it.

### Reading the process's identity

**`libc`, for `geteuid` and nothing else so far.** The socket directory is
per-user, so resolving it needs the effective uid, and there is no way to ask for
that in `std`. `libc` is the smallest thing that answers the question and is
already in the dependency graph beneath `tokio`.

### Diagnostics are off unless asked for

**`RUST_LOG` sets the verbosity; unset means nothing is logged at all.** The
daemon is a background process whose output nobody reads when things are working,
and the events it is there to carry go over a socket rather than through its log.
Anyone debugging one sets the variable.

## 2026-08-18 — daemon lifecycle

### The daemon says what it is, and `AGENTBUS_LOG` replaces `RUST_LOG`

**`agentbus daemon` logs at `info` by default; `--log-level`, or `AGENTBUS_LOG`
behind it, changes that.** This replaces "Diagnostics are off unless asked for"
above, for this command only. A daemon is a process somebody starts and then has
to reason about hours later — which build is this, which directory is it serving,
what timings was it given, why did it stop — and the two lines that answer those
questions are worth more than the silence they cost. Two lines per run is not
noise. The variable is named for this project rather than for the logging crate
because it is part of the daemon's documented interface, alongside `AGENTBUS_DIR`,
and because a supervisor that sets `RUST_LOG` for its own reasons should not
thereby reconfigure the bus.

Everything else in the workspace keeps the earlier rule, and `agentbus emit` keeps
it absolutely: that path runs inside somebody's coding agent, prints nothing
anywhere, and has no logging to configure. Colour is off unconditionally — a
daemon's stderr is nearly always captured into a file or a journal, where escape
codes sit in the middle of every field.

### Configuration is flags, with an environment variable behind each

**No config file for the daemon.** `--dir`, `--stale-secs`,
`--done-retention-secs` and `--log-level` are the whole of it, and each reads from
`AGENTBUS_DIR`, `AGENTBUS_STALE_SECS`, `AGENTBUS_DONE_RETENTION_SECS` and
`AGENTBUS_LOG` respectively when the flag is absent. Flags are for people;
variables are for whatever supervises the process, which often cannot choose the
argument vector but can always choose the environment. A file would be a third
place for the same answer to live, and a file per machine is the opposite of what
a bus that gets provisioned onto other machines wants.

### One daemon per directory, decided by `flock`

**An exclusive `flock` on `<dir>/daemon.lock`, not on the sockets.** The kernel
drops it when the holder dies however it dies, including `SIGKILL` and including a
machine losing power, which is what makes the recovery unambiguous: a daemon that
holds the lock knows no other daemon is alive in that directory, and can therefore
remove the socket files it finds there and rebind them. Locking a socket instead
would confuse "a file exists" with "somebody is listening", which is exactly the
distinction that has to be made. The pid written into the file is for humans;
nothing reads it back to make a decision, because a pid read from a file is a
guess about a process that may already have been replaced.

**A daemon that finds the directory taken exits 3.** Distinct from the general
failure code because it is usually not a failure: a caller whose goal is "a daemon
is running here" has got what it wanted, and can treat that one code as success
without having to parse a message.
