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
