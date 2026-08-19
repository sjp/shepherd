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

## 2026-08-18 — publishing the stream

### The clients that read the stream use blocking sockets

**`subscribe` and `status` use `std`'s blocking `UnixStream` and never start an
async runtime.** Following a stream is one connection read one line at a time,
which a runtime makes no faster and makes slower to start, and `subscribe` exists
to be a pipe that anything can put in front of `jq`. The runtime stays where the
concurrency is, which is the daemon. `--recent` bounds its wait with
`set_read_timeout` rather than with a timer task.

### Fan-out is per subscriber, and drops rather than waits

**Each subscriber gets a bounded queue and a task that drains it into the socket;
a line that arrives for a full queue disconnects that subscriber.** The
alternative — waiting for a slow reader — would let anything watching the bus
stall an emit, and an emit that stalls is a coding agent that hangs. The cost is
that a subscriber occasionally reconnects, and reconnecting is already correct by
construction because every stream begins with a snapshot.

**Events are published under the same lock that stamps them.** Sequence order is
what a subscriber is promised, and two connections ingesting at once would
otherwise be free to reach the publisher in the opposite order to the one they
were numbered in. The publish cannot block, so holding the lock across it costs
nothing.

### `serde` in the daemon

**The daemon takes `serde` directly, for the `Serialize` bound on the one
function that turns any stream line into bytes.** It was already in the graph
beneath `agentbus-protocol`; naming it is what lets the daemon serialize a
snapshot, an event and a heartbeat through one function instead of three.

### `thiserror` in the command-line crate

**`agentbus-cli` is a library with a three-line binary in front of it, so its
error types follow the library rule.** The one enum it defines says why a stream
could not be read, and the binary's reporter walks its source chain the same way
it walks the daemon's.

## 2026-08-18 — the emit client

### The emit path cannot produce a non-zero exit code, whatever it is given

**Every invocation naming `emit` exits 0, including one the argument parser
refuses.** The rest of the binary reports a bad command line the way every
command-line program does: usage on stderr, a non-zero status. That is exactly
what a coding agent reads as *deny the tool call the user just asked for*, so
this one command is exempt — a misconfigured hook is precisely the case where
the guarantee has to hold, and it is also the case where the argument parser is
the thing objecting. The values of `--agent` and `--source` are therefore plain
strings rather than closed sets: a value nobody has heard of is one more payload
that means nothing, which is already this command's ordinary outcome, rather
than a usage error. Whether a command line names `emit` is decided by looking at
the words rather than by asking the parser, because by then the parser has
already refused to understand them.

### The three things that could wait are each given a deadline

**100 ms for the whole invocation, of which at most 40 ms may be spent waiting
for a payload and at most 50 ms on connecting; what is left bounds the write.**
The budget is measured from the first moment the process has control rather than
from the moment the work starts, because it is a promise about how long the
agent waits and the agent is waiting from `fork` onwards. The split is what is
left over after the ordinary case: an agent writes its payload and closes, and a
daemon that is running accepts immediately, so nothing here is approached in
practice. The deadlines exist for the three ways that stops being true — an
agent that hands a hook a pipe and forgets about it, a daemon whose backlog has
filled because it stopped accepting, and a receiver that reads the first few
bytes and hangs up. Each of those, without a deadline, is a hook that never
returns.

### `socket2` for the connect, `libc` for the wait

**The emit path takes both, and the daemon's own rule — `std` blocking sockets,
no async runtime — is unchanged.** `std` cannot connect a unix socket with a
timeout and cannot make an unconnected non-blocking one, so bounding the connect
means either `socket2` or forty lines of hand-written `unsafe`. `socket2` is
already in the dependency graph beneath `tokio`, and having less `unsafe` on the
one path in this repository that must never break is worth more than having one
fewer name in the manifest. `libc` earns its place separately, for `poll` on the
payload's descriptor: the alternative is to read it on another thread, and a
thread is outside the panic guard that makes this path total.

### Diagnostics here are a switch, and may be sent somewhere they survive

**`AGENTBUS_LOG` turns them on — any value but `off` — and `AGENTBUS_LOG_FILE`
sends them to a file instead of stderr.** The daemon reads the same variable as
a filter; this command has nothing to filter, so it reads it as the question it
can answer. The file exists because of where this runs: an agent commonly
discards its hooks' stderr, which makes stderr the one place a person debugging
an installation cannot look. Silence remains the default, and a panic is routed
through the same switch rather than through the default panic hook, so that an
agent showing its user its hooks' stderr never shows them a crash report for
something they did not run.

### A build with debug assertions on carries an agent that panics

**One name in the adapter dispatch, present only when `debug_assertions` is on,
panics when it is asked to normalize anything.** The guarantee that a panic
anywhere below still exits 0 with empty stdout is worth very little tested
against a closure standing in for the real process, and there is otherwise no
way to make the real process panic on purpose. A released build does not contain
the arm.

## 2026-08-18 — installing hooks

### Rewriting somebody's config file preserves the order of their keys

**`serde_json` is taken with `preserve_order` on.** The installer's central
operation is a rewrite of a file a user maintains by hand: their entries are read
in and written back out around ours. Without this feature `serde_json` holds an
object in a `BTreeMap` and hands the keys back in alphabetical order, so the
first install would reorder every object in the file and the diff would be the
whole document. The cost is one more crate in the graph — `indexmap`, beneath
`serde_json` — and it is confined to nothing: the feature is global to the
workspace, which is safe here because nothing in this repository depends on
object key order, and every equality test on a `Value` is order-independent
either way.

### A file that repeats an object key is refused rather than rewritten

**The installer reads JSON with a deserializer that rejects duplicate keys,
where every other reader keeps the last.** Keeping the last is the right
behaviour for a reader and the wrong one for a rewriter: the losing keys would
disappear from a file the user wrote, silently, as a side effect of installing a
hook. Refusing costs about sixty lines of visitor and is the only way to keep the
promise that everything not ours comes back out unchanged. The round-trip check
the same function performs — serialize, read again, compare — is a second
guard, and with the strict reader in front of it there is no input known to
trigger it; it is cheap and it is the thing that would catch a future change to
either library.

### Backups are stamped with a count, not a date

**A backup is `<name>.agentbus-backup-<milliseconds since the epoch>`, and the
newest three are kept.** The only thing anything does with the stamp is put the
copies in order, and a plain integer needs no calendar to produce and none to
compare — this crate would otherwise be the second place in the workspace
carrying civil-calendar arithmetic. The stamp is taken as the greater of the
clock and one past the newest backup already there, so that several copies taken
inside one millisecond still order correctly and a stamp freed by rotation is
never reused.

### `tempfile` is a dependency of the installer, not just of its tests

**Every write is a complete file renamed over the target, and the temporary file
is made in the target's own directory.** A rename is only atomic within one
filesystem, so anywhere else would be a guess about how the machine is
partitioned. `tempfile` was already in the workspace for tests; using it here
buys cleanup of the temporary file when a write fails partway through, which is
the case the whole arrangement exists for.

### What has been installed is recorded, and it records exactly one thing

**`~/.local/state/agentbus/installed.json` (`XDG_STATE_HOME` honoured) lists the
files written to and whether this program created each of them.** It is not a
manifest of what is installed — the files themselves are that, and they carry the
mark that says who wrote each entry, so a record that disagreed with them would
be worse than none. It exists for the one question the files cannot answer: with
our entries taken out and nothing left, is the empty file litter this program
made, or is it the user's own file that they asked us to add to? The first is
deleted and the second is kept.

## 2026-08-18 — installing into Claude Code

### Claude's plugin is offered through a generated marketplace of one

**`agentbus install --agent claude` writes a marketplace directory under the
data directory and then runs `claude plugin marketplace add` and `claude plugin
install agentbus@agentbus -s user`.** Claude installs plugins from marketplaces
and only from marketplaces: `claude plugin install` takes a plugin name, not a
directory, and refuses a path with *not found in any configured marketplace*. A
marketplace is itself only a directory with a manifest listing the plugins in
it, so wrapping the one plugin in one is a few lines of generated JSON and no
change to how the plugin itself is built.

The consequence has to be stated plainly, because it is the one thing this
arrangement cannot deliver: **Claude writes `~/.claude/settings.json` itself**,
recording the marketplace and the enabled plugin there, and leaves the two keys
behind as empty objects after an uninstall. This program never opens that file
— an integration test pins that — but a user comparing it before and after will
find it changed. The alternative is to write it ourselves, which is worse in
every way: it is the file the user maintains by hand, and doing Claude's
bookkeeping for it would mean guessing at a format that is Claude's to change.

Rejected alongside: dropping the plugin into `~/.claude/skills/<name>/`, which
Claude auto-loads with no settings file written at all. It is the tidier
mechanism and it works, but it is not what `claude plugin install` manages —
such a plugin cannot be updated or uninstalled through the plugin commands, only
by deleting the directory — so an installation made that way is invisible to the
tooling a user would reach for.

### An installation step can be a command, not only a file

**`Change` covers running somebody else's tool alongside writing, rewriting and
removing files.** The rule that a run is worked out in full before any of it
happens — so that `--dry-run` and a real run are the same code stopped at
different points — only holds if a step can stand for everything an installation
does. An agent whose plugins are registered by its own command line cannot be
installed for by writing files alone, and modelling that as something outside
the plan would mean the plan no longer described the run.

Two ways of running are kept apart. A command that changes something is part of
the installation and its failure is the installation's failure, reported with
the command line in it so a user can finish the job by hand — which is exactly
what happens when an agent's configuration directory is on the machine but its
command is not on the `PATH`: the files are written, the record is saved, and
the message says what to run. A command that only asks something decides whether
a step is needed, and an unanswered question means taking the step, because
every step asked about is safe to take again.

### A stale copy is detected by comparing it, and refreshed by removing it first

**Claude takes its own copy of a plugin when it installs one and refreshes that
copy only when the version changes.** So a reinstall at the same version is a
no-op even when what would be generated now differs — which is precisely the
case that matters, because the hooks name this program's binary by an absolute
path and a binary that moved leaves an installation pointing at nothing. The
installer therefore reads Claude's copy back, from the `installPath` Claude's
own `plugin list --json` reports, and compares it with what it would generate;
when they differ it uninstalls and installs again, which is the only sequence
that makes Claude take a new copy.

### Generated files live in the data directory, the record in the state directory

**`~/.local/share/agentbus/` (`XDG_DATA_HOME` honoured) holds what is generated
for the agents to read; `~/.local/state/agentbus/` keeps the record of what has
been installed.** They are different kinds of thing: one is the installation
itself, read by another program long after the installer exited, and the other
is bookkeeping nobody but this program reads. An uninstall removes its own
generated directory and then removes the data directory too if nothing else is
left in it — not reported as a change, because an empty directory held nothing,
but the difference between having uninstalled and looking like it.

## 2026-08-18 — the second agent: Codex CLI

### Codex is installed by dropping into `hooks.json`, never into `config.toml`

**`agentbus install --agent codex` writes `~/.codex/hooks.json` and nothing
else.** Codex documents two places hooks may be configured: that file, and a
`[hooks]` table inside the `config.toml` a user keeps their own settings in. The
JSON file wins on both counts that matter. It is often absent, and a file that
is not there is written from nothing — no merge, no user content, nothing that
can go wrong. And where it does exist it is JSON, so the round-trip guard and the
marked merge already built for that shape apply unchanged, whereas rewriting
TOML would mean losing a user's comments to prove a point about tidiness.

This is the first installation that goes through the merge rather than
generating a directory of its own, and it needed nothing added to it: the
entries are the agent's own documented shape, marked, appended to the array a
path names.

### The envelope did not move to accept a second agent

**Adding Codex changed no public type in `crates/protocol`.** That was the test
being run, not a happy accident: an envelope that needed widening for the second
agent would have been an envelope shaped around the first. What it did take is
one adapter module of about seventy lines and one entry in the emit client's
dispatch.

Two mappings are worth recording because Codex offers something Claude does not.
`PermissionRequest` means exactly "waiting on a person" and becomes `blocked`
outright, with none of the discrimination Claude's general-purpose
`Notification` needs. And Codex reports compaction from both sides: `PreCompact`
and `PostCompact` are one `compact` kind told apart by `detail.phase`, rather
than two kinds, because what happened is the same thing and a subscriber that
only cares that it happened should not have to know there are two spellings.

## 2026-08-19 — reading the process table

### The procfs reader is written here rather than taken from a crate

**`std::fs` over a root path, and no new dependency.** The published crates that
read `/proc` parse all of it — every field of `stat`, `smaps`, `net`, the cgroup
tree — behind an error type per file, and the daemon wants six fields, an
argument vector, an environment block and a link target, each of which it wants
to be allowed to fail. Taking one would mean adopting a large parse surface and
then translating its errors back into the "no answer" this code is built around.
What is written instead is a couple of hundred lines, and is the part that has
to be got exactly right anyway: the `stat` line is read from its last `)` backwards
rather than split on whitespace, because a process may legally be called
`my (weird) proc`, and no amount of dependency avoids having to know that.

### The root of the process table is an argument, never a constant

**A reader is constructed from a path; the daemon passes `/proc` and a test
passes a directory of files it wrote.** Every case worth testing here — a name
with parentheses in it, a foreground group that exited between two reads, an
`environ` the daemon is not allowed to open, a pipeline whose group leader has to
be picked out of three processes — is a state a machine is in for a few
milliseconds and cannot be asked to reproduce. Against a directory they are all
just files. This is also what makes the module compile and pass its own tests on
macOS, where there is no procfs at all: nothing is conditional on the platform,
and a root that is not there is a process table with nothing in it.

### Nothing that fails here produces an error

**Every operation answers `Option`, and writes one debug line saying which file
it was and what the system said.** A caller sampling the process table is reading
something that changes underneath it: a pid that vanishes between listing the
table and reading its `stat` is the ordinary case, not a fault, and so is an
`environ` belonging to another uid, which needs `CAP_SYS_PTRACE` to open. An
error type would put a decision in front of every call site for a condition that
has exactly one sensible response, which is to have no observation this time.
