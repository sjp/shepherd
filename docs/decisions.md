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
`--done-retention-secs`, `--assert-hold-secs` and `--log-level` are the whole of
it, and each reads from `AGENTBUS_DIR`, `AGENTBUS_STALE_SECS`,
`AGENTBUS_DONE_RETENTION_SECS`, `AGENTBUS_ASSERT_HOLD_SECS` and `AGENTBUS_LOG`
respectively when the flag is absent. Flags are for people;
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

**One name in the emit path's agent handling, present only when
`debug_assertions` is on, panics when it is asked to normalize anything.** The guarantee that a panic
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

## 2026-08-19 — publishing what the process table says

### Observations travel on the stream that carries events, numbered with them

**One broadcast channel, one sequence counter, one order.** A `foreground_change`
is stamped with the next `seq` under the same lock an event is stamped under, and
published from inside it. Two channels would have been simpler to write and would
have broken the one promise a subscriber is given — that it reads the stream in
`seq` order — because two senders are free to reach a subscriber in the opposite
order to the one they were numbered in. Numbering observations from the same
counter also means the rule that makes a stream a continuation of its snapshot
rather than an overlap with it — *skip what is at or below the snapshot's `seq`* —
needs nothing added to it.

### The `foreground` key is absent, not empty, when nothing is watching

**Whether a daemon can read a process table is settled once, when it starts.** A
daemon that can says so from its first snapshot, when the answer is still `[]`; one
that cannot — another operating system, a root that is not there, a table it may
not list — never writes the key at all and never publishes a `foreground_change`.
The two are different facts: "nobody is looking" and "nobody is running anything".
Deciding it once rather than per snapshot is what makes the absence stable, so a
subscriber that saw the key once can rely on seeing it for the life of that
connection.

### Looking at the process table happens on a blocking thread

**Each pass is handed to `spawn_blocking` rather than run on a runtime worker.**
In the steady state a pass is a couple of file reads per correlated shell and would
not be worth the hand-off; every few seconds it is one read of `environ` per
process on the machine, which is bounded but not small. A worker parked in that is
a worker not accepting the connection a hook is waiting on, and the hook is the one
caller in this system that cannot be kept waiting.

### The root of the process table is a hidden flag

**`--proc-root`, with `AGENTBUS_PROC_ROOT` behind it, hidden from the usage
text.** A machine has exactly one process table and its path is not a choice, so
this is not an option in the sense the other flags are; it exists because the
reader was built to take a root and a test can then write a process table as files
and hold it still. It is the fifth entry in the list "Configuration is flags, with
an environment variable behind each" gives above, and it follows that rule so that
there is one way to configure a daemon; it is hidden so that it does not read as
something a user is expected to set.

### `agentbus foreground` has three exit codes and the middle one is why

**0 printed something, 1 the filter matched nothing, 2 there was nothing to ask.**
A correlation with nothing running in it is *news* — the shell is real and its
terminal is idle — and a script has to be able to tell it from a daemon that is not
there or cannot look. So every way of failing to get an answer at all, including a
daemon that is not watching a process table, shares the one code, and the general
failure code is spent on the answer instead.

## 2026-08-19 — getting the binary onto another machine

### Static Linux binaries are linked with the toolchain's own linker

**`rustup target add` plus `linker = "rust-lld"` in `.cargo/config.toml`, and no
cross-compilation tool at all.** The musl targets exist because a copy of this
program is pushed onto machines with no toolchain, no package manager anyone can
rely on and no matching libc, so what is shipped must need nothing from the far
end. Rust's musl targets already carry a static libc; the one thing missing is a
linker that can produce an object file for an architecture other than the one it
is running on, which the machine's `cc` cannot and which the toolchain ships
anyway. `cross` and `cargo-zigbuild` both do the job and both are a thing to
install and keep installed, on every developer's machine and on the runner;
naming a linker is a two-line file that makes

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

work from any Linux host, which is what `scripts/build-release.sh` does one
target at a time. The result is a static PIE, and the two absences that matter
are checked rather than assumed: no interpreter, no `NEEDED` entry.

The rule this puts on the workspace is worth stating on its own: **nothing here
may take a dependency that needs a C toolchain to build.** Everything in the
graph today is pure Rust, and a dependency that is not would be discovered as a
broken release rather than as a broken build.

### An asset is named by version and triple, and by nothing else

**`agentbus-<version>-<triple>`, no extension.** Provisioning searches for a
binary, asks it what it is, and compares the answer byte for byte with the
version it expected; the name has to carry the same two facts that decide
whether a copy is the right one, and no third fact — a build number, a date, a
commit — that would make two identical binaries look different. Darwin builds
are made when a release is cut because they cost one line each, and they are a
convenience for people whose own machine is a Mac: no feature of the bus asks
for them, and they are not static.

### The manifest is a fixed shape with a version of its own

**`{"v": 1, "name", "version", "assets": [{"triple", "url", "sha256", "size"}]}`,
generated by `scripts/make-manifest.sh` and published beside the assets.** It
exists for the case where the local machine cannot produce the binary the far end
needs — an arm64 laptop has no x86_64 Linux build to push — and it is read by
something that was compiled long before it was written, which is why the shape
carries `v` and why the generator is a script in this repository rather than a
few lines living only inside a workflow file. `sha256` and `size` describe the
bytes as they will be downloaded, so a truncated fetch is caught before anything
is executed. Assets are listed in triple order, so regenerating a manifest for
the same directory produces the same file.

### One version number, checked in three places

**A release is a `v<semver>` tag; the tag must equal `[workspace.package]
version`, and the binary must print it.** The exact `--version` answer is the
mechanism that decides a provisioned copy is current, so a release whose parts
disagree makes every remote binary look stale forever. The tag is compared with
the manifest at the start of the release; each built asset is then run and its
answer compared byte for byte, natively where the runner can and under an
emulator where it cannot; and a test in the workspace pins that what the binary
prints is what `[workspace.package]` declares, so the drift that would otherwise
only appear at release time — a crate that stopped inheriting the workspace
version — fails an ordinary test run.

## 2026-08-19 — reaching another endpoint

### What a transport is, and what a far end's paths are

**One object-safe trait: run a command, copy a file in, say what the machine is,
say how long to wait before retrying, plus a name and an identity.** Anything
that holds these holds several at once, of kinds settled at run time, so the
whole surface is dispatched dynamically and nothing above it is written against
a particular kind of endpoint. The identity is separate from the name because
several names may reach one machine and deduplication has to be done on the
machine.

**A far end's paths are `String`, not `Path`.** They are resolved by a
filesystem this process cannot see, so the type that means "a path this program
could open" is the wrong one: using it invites code that canonicalizes, joins or
stats a name that only means something somewhere else. Only the *local* side of
a copy is a `Path`.

### The build's target triple comes from a build script

**`crates/daemon/build.rs` re-exports Cargo's `TARGET` as `AGENTBUS_TARGET`.**
Whether the executable that is running can be handed to another machine is a
question only the compiler can answer, and Cargo tells a build script and nothing
else. Two lines of build script beat every runtime guess.

### Whether a copy can be pushed is decided loosely and verified exactly

**The push is offered when the far end's operating system and architecture match
this build's; whether it is *right* is settled over there, by `--version`.** A
`uname` answer of `Linux x86_64` says nothing about which libc is installed, so
an equality test against a musl triple would refuse to push from every ordinary
`-gnu` build, including every development build; and a looser test cannot be
wrong in a way that matters, because a copy that arrives and cannot run fails the
version check exactly as a truncated one does. That check is the only thing that
licenses execution, and it is the same check whether the copy was pushed, fetched
or already there.

### The bootstrap is told apart by one line, read and put back

**The driver reads one line of the far end's stdout, and either recognizes the
script's `need=` or hands the stream on with that line restored.** The script
execs on success, so there is no exit status to wait for and no way to know what
happened without looking; and the stream belongs to the caller, so what was
looked at has to go back. Deciding on a timeout instead would make a slow far end
indistinguishable from a failed one.

### Backoff carries its jitter but does not draw it

**`Backoff` is four numbers and two pure functions; the caller supplies the
sample in `0..=1` that spreads a delay.** A backoff that reaches for a random
number is a backoff no test can assert anything about, and a workspace that
needs randomness in exactly one place should not take the dependency for four
lines of arithmetic.

### A daemon that outlives its subscriber is made by forking twice

**`agentbus subscribe --ensure-daemon` spawns the daemon with a second `fork`
and a `setsid` between the fork and the exec.** The state a daemon holds is the
reason to have one — an agent that said it was blocked before a connection
dropped never says so again — so a daemon that died with the attachment would
lose exactly what the bus exists to show. The second fork means the daemon is
nobody's child, so it survives the subscriber and leaves no zombie behind if it
does not; the new session means the `^C` that stops the subscriber is not also
sent to the daemon. What is started is an ordinary daemon: `agentbus daemon` is
unchanged, and nothing about the process it becomes records who asked for it.

**Whether a daemon is already there is answered by connecting, not by testing
the lock.** The caller is about to connect anyway, and the lock answers a
question one step removed from the one being asked: a daemon that holds the lock
and has not yet bound its sockets would pass a lock test and fail the connection
immediately afterwards. Connecting, then starting one, then waiting for the
socket to answer covers that case and the race between two callers with the same
code, since the loser of the race for the lock exits at once and both callers
wait for the same socket.

## 2026-08-19 — fetching a binary this machine has not got

### The http client is `ureq`, and its cryptography is Graviola

**`ureq` with rustls, and `rustls-graviola` in place of the usual provider.**
A blocking client is what this needs — the fetch happens once, on the way to
provisioning an endpoint, on a thread that has nothing else to do — and `ureq`
is the smallest one that speaks TLS.

The provider is the interesting half. rustls' default, *ring*, and its other
mainstream provider, `aws-lc-rs`, are both C, and taking either would break the
rule recorded above: `cargo build --target x86_64-unknown-linux-musl` on any
machine that is not already an x86_64 Linux one stops at `failed to find tool
"x86_64-linux-musl-gcc"`, which is a release that cannot be cut rather than a
warning. Graviola is Rust and assembly with no build script, it covers exactly
the architectures a release is built for, and with it both musl targets still
cross-compile from a checkout and a `rustup target add`. It is a young library
and that is the price: the alternative was a C toolchain per architecture on
every machine that builds a release, which is the thing that decision set out to
avoid.

The provider is installed as the process default on the first fetch rather than
configured per request, because that is the stable, documented way to do it —
`ureq`'s per-agent hook is explicitly outside its semver promise — and because
one process here has exactly one TLS user.

### A release is one base, and everything is read from beside the manifest

**`<base>/manifest.json` and `<base>/<asset>`, where `<base>` defaults to where
this version was published and `AGENTBUS_RELEASE_BASE` replaces it.** The
manifest's own `url` field says where its publisher put each asset, and what is
actually read is the copy beside the manifest that was just read — the same
location when nobody has overridden anything, and the mirror's copy when
somebody has. That is what makes a mirror a copy of a directory rather than a
service to stand up, and it is why a `file://` base works at all: an air-gapped
site copies four files and a directory, and an offline test is the same thing in
a temporary directory. The `url` is still what names the asset, so a later
release may rename its assets without anything here being taught the new scheme.

### The repository is one constant, and it is expected to move

**`release::REPOSITORY` is the only place the publishing location is written
down.** The default base, the manifest's location and every asset's location are
derived from it and the version, so moving the releases is that line plus a
release that puts the assets in the new place. Anyone who has not moved them can
still point a single run elsewhere with `AGENTBUS_RELEASE_BASE`, which is the
supported way to use a mirror without rebuilding.

### The manifest is cached beside the binaries it describes

**A verified asset and the manifest it was verified against are kept together
under `<XDG_CACHE_HOME>/agentbus/<version>/`, and a second run makes no request
at all.** The cached binary has to be checked before it is sent — a cache is a
directory anyone can write to, and what comes out of it is about to be executed
somewhere else — and checking it means knowing what it should hash to, which is
what the manifest says. Fetching the manifest again to learn that would make
"already fetched" cost a round trip, and a release's description of itself does
not change once published, so the copy taken at the time is the copy to check
against. It is kept verbatim rather than rewritten from what was parsed, so a
later build reading the same cache sees what the publisher actually wrote.

Anything that fails that check — a truncated binary, a manifest for another
version, a file somebody edited — is removed and fetched again, once. The
download itself lands under a `.part` name and is renamed only after its length
and hash match, so a run that is killed halfway leaves nothing a later run could
mistake for a binary.

### Fetching is what happens when pushing cannot, and never instead of it

**A far end whose operating system and architecture this build could run keeps
getting this running executable; only the rest are fetched for.** Pushing needs
no network, no release to have been published and no cache, and it is the path
that works on a laptop with no connectivity provisioning a container on the same
machine. The fetch exists for the case the push cannot cover at all — an arm64
Mac has no x86_64 Linux binary inside it — and what it produces is checked twice:
against the manifest here, and by `--version` over there, which is the same
check a pushed copy faces.

## 2026-08-19 — being told which endpoints to attach to

### The control path is a watched file, not a third socket

**A daemon polls `targets.json`'s modification time every two seconds, and
`SIGHUP` brings the next look forward.** The bus has two sockets and they are
deliberately single-purpose — one takes events in, one sends the stream out —
so there is no channel on which a client could ask a running daemon to do
something, and adding one would mean designing a request protocol, versioning
it, and answering what a half-applied request means. A file needs none of that
and is better suited to what is actually being said: which endpoints somebody
wants attached is a fact that outlives every daemon, so the machine that was
switched off last night is still wanted when it comes back, and a daemon
starting from nothing finds out what to do by reading rather than by being told
again. It also settles the question of who may declare one: a person at a
shell, a configuration management system, and a program that noticed one of its
terminals go somewhere else all leave the same three fields behind, and nothing
in the daemon can tell which of them wrote it.

`SIGHUP` is what it has meant to a daemon since long before this one: read your
configuration again. Installing the handler changes it from the default
disposition, which would have ended the process.

### Two files, in two directories, meaning two different things

**`targets.json` under the user's configuration directory says what somebody
asked for; `attachments.json` beside the sockets says what a daemon is doing
about it.** They are separated because they have different lifetimes and
different owners. A declaration is edited by a person and survives reboots; what
came of it is made by one daemon as it starts, rewritten whenever an attachment
changes state, and taken away when that daemon stops. That last property is what
lets `agentbus targets` answer without a daemon running and without connecting
to anything: no file means no daemon, an empty list means a daemon attached to
nothing, and the two are genuinely different answers. A daemon killed outright
leaves the file behind exactly as it leaves its sockets behind, and the next
daemon to claim the directory clears both.

Both are `{"v": 1, …}` and the version is read before the shape, so a file a
later build wrote is reported and left alone rather than overwritten — which
matters most for the declarations, because overwriting them would discard what
somebody asked for.

### A declaration is a transport's name and its words, kept verbatim

**Two declarations are the same one when the transport and every word match,
element for element.** Nothing above the transport parses what it was given: what
`ssh` accepts is settled by the `ssh` on the machine that will run it, and a
daemon that tried to normalize an argument vector would be re-implementing
somebody else's command line badly. Deduplicating two names that turn out to
reach one machine is a different question, answered by whichever transport can
answer it, and reflected in the aliases it reports rather than by rewriting the
file.

On the command line the one thing that has to be decided is which word is the
transport. A first word naming one names it (`attach docker eager_mclean`,
`attach ssh -- fileserver`); anything else is the arguments of the transport
that needs naming least (`attach -- -p 2222 bob@host`). The cost is a host whose
entire argument vector is the word `docker`, which is declared as `attach ssh --
docker`, and it buys a command line nobody has to read the manual for.

### Reconciling is a thread, and it is the thing that owns the attachments

**One thread, sleeping between passes, holding every attachment it started.** A
pass reads a file, may start an attachment, and may stop one — and stopping one
waits for the thread reading that endpoint to finish — so none of it belongs on
a runtime worker, where the cost of being wrong is a hook waiting on a
connection nobody is accepting. Holding the attachments there rather than in the
daemon's async state is what makes the shutdown order obvious: the reconciler is
dropped first, which detaches everything and withdraws the sessions those far
ends were speaking for, and only then are the files removed.

A pass is a comparison rather than a sequence of edits: start what is declared
and not attached, stop what is attached and no longer declared, leave the rest.
It depends on nothing it did last time, so it is safe to run at any moment, and
a file that cannot be read at all leaves every attachment exactly as it was —
somebody halfway through editing their declarations is not a reason to tear down
a connection that is working.

### A transport is built from a name through a registry, and an unknown name is ignored

**`Registry` maps the name in a declaration to a function that builds a
transport, and answers "nothing" for a name it has never heard of.** A
declaration may have been written by a later build or by somebody who guessed,
and the only sensible response is to leave that one alone, say so once, and
carry on with the rest. A name it does know that cannot be turned into a
transport is a different answer — a state to report, carrying the transport's
own sentence about what is wrong with the declaration, and not a thing to retry
every two seconds, because nothing will be different until somebody changes what
they declared.

### `tempfile` and `serde` join two more crates, for the same reasons as before

**`tempfile` is a dependency of the daemon rather than only of its tests**,
because both of these files are written as a whole file renamed over the target
and the temporary one has to be made in the target's own directory — the same
rule, and the same reasoning, as the installer's. **`agentbus-cli` takes `serde`
directly** for the `Serialize` on the merged structure `agentbus targets --json`
prints, which is a shape this crate owns rather than one the protocol defines.

## 2026-08-19 — reaching a container

### Containers are found rather than declared, through a trait of their own

**`Discovery` sits beside `Transport`: a transport that has an authoritative
list of its own endpoints reads it, and the reconciler drives that on the
transport's own cadence.** Docker keeps such a list and ssh does not, and that
asymmetry is a fact about the two rather than an inconsistency to be tidied
away. Making it a trait rather than a branch in the reconciler keeps the loop
between the two files free of any knowledge of Docker, and keeps one writer of
`attachments.json`, which is what makes something found appear in `agentbus
targets` beside something declared without a second mechanism.

A discovery is told the working directories this daemon's own sessions reported
and the declarations already made, and may use the first only to order and name
what its list already said. A bus that found fewer containers the less its
subscribers happened to say would be one whose aggregation depended on who was
watching, and the whole arrangement exists to avoid exactly that.

### Every running labelled container is attached, and a declaration wins over a find

**Being up and carrying `devcontainer.local_folder` is the whole test.** A
Compose project builds several containers from one directory and each is a
machine of its own, so there is no sense in which one of them is the project's
container; walking up from a working directory to find a devcontainer
definition settles which project a container belongs to, and that decides the
*order* they are reached in and nothing else. Where a declaration and a find
name one container the declaration wins, because somebody asked for that one and
nobody asked for the other — which is also what makes `agentbus install docker`
worth having for a container that carries no label at all.

### A container is addressed by whatever it was called and identified by its id

**Commands go to the word somebody used — a name, or as much of an id as they
copied — and what gets written down as the far end's identity is the full id,
asked for with `docker inspect` once and remembered.** Docker resolves both, so
sending to the word that was used is what was asked for; a name is the wrong
thing to deduplicate two views of one container by, because Docker will give the
same name to a different container tomorrow. The question is asked when the
first command is sent rather than whenever somebody wants the answer, so that a
container which is not there does not cost a process every time a loop comes
round.

### The transport is told when a copy is running at the far end

**`Transport::established` is called with the version whenever the provisioner
has a copy of this program running over there.** Some far ends need more than a
binary to be worth having: a container needs the agents inside it wired up to
the daemon that has just been started in it, or an agent started in there is
invisible to somebody who may not know the container exists. Nothing is required
of the hook and nothing is reported by it — what it does is beyond what the
caller asked for, so failing at it must not fail the attachment.

Writing hooks into a container without asking is deliberate and does not
generalise. The file it lands in was made by an image and goes away with it,
which is the opposite of the position on somebody's own machine.

### Docker is a command line, not a library

**The daemon runs `docker` and reads what it prints.** API version negotiation,
the socket's location, credentials and the rootless variants are then somebody
else's problem for as long as the command line stays the one everybody
implements — including the things that answer to it without being Docker, which
`AGENTBUS_DOCKER_BIN` is there for. The listing is asked for tab-separated
rather than as JSON because Docker flattens labels into one comma-joined string,
and the label being read holds a filesystem path: a path with a comma in it is
ordinary, and a path with a tab in it is not.

### Provisioning an endpoint is a subcommand of `install`, and the local flags are refused with it

**`agentbus install docker <container>` acts on a container; `agentbus install`
with no endpoint acts on this machine.** `--agent` and `--dry-run` choose which
of *this* machine's agents to act on and whether to act at all, and neither means
anything about a container — what goes in is whatever turns out to be in there,
and there is no halfway version of putting a binary on a machine. A command line
carrying both is refused with the parser's own usage status rather than quietly
doing half of what it says.

## 2026-08-19 — knowing which daemon is at the far end

### A daemon is the machine it is on and the user it runs as

**Every snapshot carries `daemon: {"id": "<machine-id>:<uid>"}`, and the string
is compared for equality and read for nothing else.** The machine half is
whatever the host already calls itself — `/etc/machine-id`, then D-Bus's copy of
it — because an id somebody else maintains outlives reboots, addresses and this
program's own installation. The user half is not decoration: this program's
sockets are per-user, so `ssh root@host` and `ssh host` reach one machine and
two daemons holding two sets of sessions, and keying on the machine alone would
merge them.

A host that names no machine of its own gets one made up and kept beside the
user's runtime files. It is written there rather than beside the sockets because
a caller may point a daemon's sockets anywhere — a test, a second bus — and an
identity that moved with them would make one machine look like several. Where
that directory is cleared at boot, the id lasts for the life of the boot, which
is longer than any attachment to that machine lasts.

### Randomness for that comes from the kernel, not from a crate

**Sixteen bytes of `/dev/urandom`, written as hexadecimal.** It is wanted in one
place, for one value, once in the life of a machine that has no id of its own,
and a dependency is a thing to be justified by more than four lines of code. A
process that cannot open it — one with no `/dev` — falls back to hashing what is
to hand, which is worth less than randomness and worth more than every such
daemon agreeing on one value.

### An address is not an identity, and the guess is kept apart from the answer

**`Transport::identity` is what the transport knows it reached; a new
`Transport::way_in` is what it could tell before reaching anything.** Docker
answers the first, with the container id it asked for. ssh answers only the
second, because where ssh would go is not what is at the other end of it: two
names may be one machine, one name may be two machines on two days, and the
party that knows is the daemon over there. What `agentbus targets` prints
follows the same distinction — an identity when there is one, and the word
`(provisional)` while the only thing known is where a connection would go.

The way in is ssh's own resolved user, host and port, which is what ssh builds
`%C` — and so the multiplexed connection's socket — out of. That is why one
string does both jobs: two declarations that answer alike are two declarations
ssh itself considers one endpoint, and they are already sharing one connection.
Nothing here reproduces ssh's hash; what is compared is the resolution ssh made
it from.

### Several names for one daemon are all read until it says they are one

**Every declaration gets its own stream, and the ones reading a daemon another
stream is already reading are let go of afterwards.** Grouping them beforehand
on the way in makes `agentbus targets` honest before anything has answered, and
is only a guess — the split case is real, and a guess that suppressed a
connection could never be found out. Riding ssh's multiplexing is what makes
that affordable: the second stream costs a channel on a connection that is
already open, not a second login.

Letting go of the extra stream is not detaching. Detaching says nobody can speak
for those sessions any more, and here the other attachment still is, so what was
reported stays exactly where it is; and the connection is left open when
something else is still reaching through it, which is what `Transport::keep_open`
is for. The same rule now applies to ordinary detaching, where two attachments
happened to overlap: a session is ended when no attachment is reporting it, not
when the first of two goes away.

## 2026-08-19 — where a copy of this program lands on somebody else's machine

### The far end decides, and says so; this end composes no paths at all

**Two shell fragments are prepended to every script that names a path over
there, and what they resolve is read back.** Where an installation goes depends
on `AGENTBUS_REMOTE_BINARY`, `XDG_BIN_HOME`, `XDG_DATA_HOME` and `HOME` — every
one of them a variable only a shell running on that machine can read — so a
second answer worked out on this side would be free to disagree with the one the
script that does the writing will use. It very nearly did: the same three paths
were written out in four places that had to be kept in step by hand, and the
search's candidate list was a fifth. Now `find-installation.sh` reports the
paths it resolved and the provisioner uses those, whatever they turned out to
be.

`AGENTBUS_REMOTE_BINARY` names the whole path rather than a directory, because
that is what it already meant everywhere else: it is the head of the search's
candidate list, and it is what the refusal for an occupied path tells somebody
to set. Letting it decide the write as well is what makes it impossible for the
search and the installation to disagree about where the copy is.

The default is unchanged and is not a thing to change: `~/.local/bin` is the
per-user executable directory in systemd's `file-hierarchy(7)`, it is what a
stock `~/.profile` on Debian and the profile scripts on Fedora put on the `PATH`
when it exists, and the alternative that needs no variable — `/usr/local/bin` —
needs root on a machine somebody may only have an account on.

### A copy that is only being borrowed goes somewhere per-user

**`$XDG_RUNTIME_DIR/agentbus`, then `/tmp/agentbus-$(id -u)`, which is the rule
this program's own socket directory already follows.** The old answer was
`/tmp/agentbus-<version>`, flat, and shared by everyone on the machine, which is
wrong twice over on exactly the multi-user hosts this is aimed at. It is wrong
about ownership: the second user to provision a host at a different version
cannot rename onto the first user's file, and cannot sweep it either, because
`/tmp` is sticky. And it is wrong about trust: the only gate before the script
`exec`s a candidate is that it answers `agentbus <version>`, and at a fully
predictable path under a world-writable directory that is a file anybody on the
machine can put there first.

So the directory is per-user, and being per-user is checked rather than assumed
— **the candidate is considered only when the directory turns out to be a
directory this user owns that nobody else may write.** `mkdir -p` succeeds
against a directory somebody else created, and a `chmod` on one fails without
saying so, which is why resolving the name is not on its own worth anything. The
owner is read out of `ls -ldn`'s numeric third field rather than asked for with
`find -user`, which takes a name or a uid and prefers the name, so on a machine
with a user called `1000` it answers a different question than the one being
asked.

**The search still writes nothing, including that directory.** A far end that is
already current has to cost one round trip and no writes at all, and a search
that made somewhere to put a copy would break that for every attachment. The
directory is created by whatever is about to put a file in it, and the sweep of
superseded copies is worked out over there for the same reason — it runs on the
path where a copy was found, and must not be the thing that leaves a mark.

`AGENTBUS_DIR` is deliberately not consulted for this. It moves the sockets, and
a caller may point it anywhere for a test or a second bus; a binary that moved
with it would be a surprise nobody asked for.

### A transport asks its far end where that is, once, and remembers

**The same shape as a container's id: asked when the first command is sent, kept
for the life of the transport.** `install_path` is called from places that have
made no round trip — the sweep, and the two commands a container is sent after
it is established — so the directory cannot be an argument threaded down from
the bootstrap's answer, and asking every time would put a round trip inside a
loop that runs all day.

A far end that will not answer gets the directory copies used to go in, and the
failure is logged rather than raised: the answer has nowhere to put one, and an
attachment must not end over housekeeping. Nothing is lost by being wrong here,
because a copy that lands where the search does not look still fails the version
check, which is the only thing that licenses running anything over there.

The sweep clears both the directory copies go in now and the flat names they
used to go in, so a host provisioned by an earlier release is not left with
litter nobody can account for. It skips directories, because the old pattern now
matches the new directory.

## 2026-08-20 — the emit path reads what a payload means

### The socket is looked for before anything that could cost more than a `stat`

**`emit` asks whether a bus is listening as soon as it has the payload, ahead of
parsing it and ahead of looking for any manifest.** The promise that a machine
with nothing running costs its agents one `stat` was previously true because
there was nothing else on the path worth measuring; now that what a payload
means is read from a file, it has to be true by construction instead. Putting
the question first is what makes it so: the file reads, the TOML parse and the
lookup are all downstream of a check that returns on the ordinary machine, so no
future addition below that line can quietly start costing an agent something.
The check moved out of `deliver`, which now assumes what its caller established
— a daemon that went away in between fails on connect, which was always the same
outcome as never having been there.

### What the emit path may spend on the filesystem, in full

**Two attempts to open a file, a size check and a bounded read of whichever
exists, and one TOML parse — strictly after the socket check, and fail-soft at
every step.** The two are the tiers that can outrank the mapping inside the
binary: the copy its operator wrote and the copy fetched from a catalog. The
64 KiB cap that every other reader of a manifest applies is the cap here too. A
file that cannot be read, one over the cap, one that is not TOML and one
describing another agent all step down to the next copy and end at the copy
compiled in, so the worst a half-edited mapping can do is cost its author a
diagnostic they have to turn on to see. There are no regexes in a hook mapping,
nothing to compile, and no directory to walk. This extends the emit path's
existing rules rather than contradicting them: still no runtime, still no retry,
still nothing on stdout, still exit 0.

### The bundled mappings are parsed on every invocation

**No cache, no precompiled table, no lazily built static.** The process reads one
payload and exits; the parse is of a string already in memory and does not touch
the disk, and measuring it against the invocation as a whole puts the whole run
— payload, mapping, delivered event — around a millisecond against a 100 ms
budget. A cache would be a second representation to keep in step with the files,
for a beneficiary who does not exist.

## 2026-08-20 — a second thing an emitter can say

### The envelope's version stays 1, because every part of this is additive

**A new emit line shape, a new stream line kind and a new optional field on a
snapshot entry, and `v` is still `1`.** The number exists so that a reader can
refuse a document it cannot understand; bumping it for a change no reader has to
understand would cost every existing pairing for nothing. Each of the three
degrades on its own, in a direction that was designed in rather than discovered:
a daemon built before assertions reads one, finds no `kind`, fails the event
parse and drops that single line; a subscriber built before them reads the new
stream kind, does not recognize it and ignores it, which is the rule it has
followed since the first version; and a subscriber reading a snapshot entry with
a field it has never heard of drops the field, exactly as it drops any other.
Nothing that existed before changed meaning, and nothing already on the wire
moved, which is the whole test for whether a version has to move with it.

### Two line shapes on one socket, told apart by which field is present

**`kind` means an event; `assert` means a state assertion; both or neither means
the line is dropped.** The alternative — a tag field naming the shape — would
have made every line unreadable to a daemon that predates the tag, so a new
emitter meeting an old daemon would have lost everything rather than the one
thing that daemon could not have acted on anyway. Discriminating on presence is
what makes the addition safe with no coordination at all: nobody has to upgrade
in an order, and the degradation is one dropped line, in the one direction where
dropping is correct.

### The republished assertion carries the reasoning and not the evidence

**`detail` survives to subscribers; `raw` is dropped on the way through.**
Evidence is as large as whatever produced it felt like making it — a screenful of
text is ordinary — and an observer re-asserts a state it can still see every
second or two, so carrying it would charge every subscriber for the same
screenful several times a second per observed slot. `detail` is small by
construction and says which rule concluded what, which is what a subscriber
needs to render or debug the claim. Anything that genuinely needs to see what
was seen is better off running the observer itself, where the evidence already
is.

## 2026-08-20 — taking manifests from a catalog

### The manifest channel uses `ureq` too, behind a feature

**The same client the release fetch uses, compiled into `agentbus-detect` only
when `remote-updates` is on.** The client decision above stands and is not
re-litigated: a blocking client on a thread with nothing else to do, with
Graviola behind its TLS so that a release still cross-compiles. What is new is
where it lives. Detection is a library, and the useful thing about it is that
reading a screen needs nothing but the screen — a host embedding it to watch its
own terminals should not acquire an http client, a TLS stack and a certificate
store by doing so. So the update channel is a feature that is off by default and
that the command line turns on, and the crate without it is exactly the
network-free library it was before.

### What arrives is compared against the bundled copy as well as the cached one

**A fetched manifest must be strictly newer than every copy the machine already
has, the compiled-in one included.** The cached tier alone would have been the
obvious comparison, and it is not enough: a machine that has never fetched
anything has a bundled manifest that may already be newer than what a stale
mirror publishes, and taking that copy would write a file the store then has to
notice is old and step over. Comparing against both means the tier below is
never populated with something that could not win.

The tie is the interesting case. Two copies claiming one version have to *be*
one copy, so an equal version is accepted only when the bytes are identical —
in which case nothing is written at all, which is what makes a check that finds
nothing new cost one request and no disk. Equal with different bytes is refused
rather than taken, because one version number meaning two different things is a
fleet where no two machines can be shown to agree, and the publisher who edited
without bumping is the one who can fix it.

### A catalog is refused whole; an entry is refused alone

**Schema, path safety and duplicate listings stop the run; anything about a
single manifest costs that manifest.** The split follows what the failure says.
A catalog in an unknown schema, one naming a path that climbs out of its own
directory, or one listing a manifest twice has been shown to be untrustworthy as
a document — reading the rest of it would be trusting a file that has just
demonstrated it should not be. A manifest that is missing, oversized, unparseable
or older is one publisher's mistake about one agent, and letting it stop the
other nineteen would turn a typo into an outage of the whole channel.

The exception in the other direction is a family this build has never heard of,
which is skipped with a note rather than refused: that is the forward
compatibility hatch that lets one catalog serve builds from both sides of a new
family being added.

### The status file counts seconds, and does not spell out a date

**`last_checked_unix` is an integer, and whatever displays it owns the
calendar.** This follows the datetime decision above rather than departing from
it: the arithmetic that turns an instant into a date lives in the crate that
needs it, and the detection library needs a number it can compare, not a
calendar. The command line already has the daemon's conversion available for the
one place a person reads it.

## 2026-08-20 — managing the manifests from the command line

### The store hands back the copy in force; the command line does not re-derive it

**`ManifestStore` keeps the text of the active manifest beside its compiled
form, and `show` prints that.** The alternative was for the command to walk the
tiers itself — read the override, then the cached copy, then reach for the
bundled one — and it would have been a second implementation of precedence that
could disagree with the first. A command whose whole purpose is to answer
"which copy is answering?" is the last place a disagreement about that could be
allowed. Holding the text costs a few kilobytes per agent, borrowed rather than
copied for the bundled tier, against a compiled manifest that was already
holding the same content in a parsed form.

### The manifest goes to stdout; where it came from goes to stderr

**`agentbus manifests show claude > claude.toml` writes a file that is
byte-for-byte the copy that was in force.** Everything about provenance — the
tier, the path, what was shadowed on the way — is commentary on the answer
rather than the answer, and the workflow this command exists for is starting an
override from the copy that is running. So the split the rest of this program
already uses is exactly right here: the bytes are the output, and the sentences
about them are diagnostics.

### A check fails only when the catalog does

**Per-manifest refusals are reported and are not a failure of the command.**
This is the exit code following the update semantics rather than inventing its
own: one manifest that could not be taken leaves every other manifest checked
and the machine's record of the check intact, which is a run that did its job.
A catalog that could not be read is the one outcome where nothing was found out
at all, and something scripting `manifests update` has to be able to tell that
apart from a quiet afternoon in which nothing was published.

### The list carries one column of sentences, and both kinds go in it

**What the store passed over and why the last check refused what it was offered
share the `NOTES` column.** They come from different places — one from
precedence, one from the status file — and they answer the same question, which
is why the copy a person is looking at is the copy they are looking at. Keeping
them in the last column keeps a sentence from pushing the aligned columns
around, and either kind being present is what marks the row as one worth
reading.

## 2026-08-20 — checking for newer manifests on a timer

### The daemon carries the clock, and still reads nothing

**A daemon fetches manifests and never consults one.** The bus is the only thing
in this program that runs long enough to notice that a file was published, so it
is where a timer belongs; but what it does with what arrives is put it on the
disk. Nothing in the daemon then has to be told: the commands that read
manifests are one-shot and open a store per invocation, and a long-lived program
embedding the store decides for itself when to reload. That keeps the property
that made the manifests worth having — the thing that reads them and the thing
that fetches them are not the same thing — and it keeps every code path that
touches an event free of manifest parsing.

### A thread with a sleep, not a task with a timer

**One dedicated thread, waiting on a condition variable between checks.** A
check is blocking http with timeouts measured in seconds and a few files written
under it, which is the same shape as the process table sweep and the endpoint
reconciler, and for the same reason it stays off the runtime's workers: a worker
parked in a network read is a worker not accepting the connection a hook is
waiting on. It is stopped by being dropped and it is not joined — it owns
nothing anybody else can see, and the one thing it might be doing is waiting out
a request timeout, which is not a wait worth making shutdown carry.

### On by default, with the flag named for the thing and not for its negation

**Checks are on, `--no-update-manifests` turns them off, and
`AGENTBUS_UPDATE_MANIFESTS` is the variable behind it.** Detection data that is
only current on the machines whose owners remembered to ask is data nothing can
rely on, and the whole point of moving detection into files was that a UI change
could be answered without a release reaching every machine by hand. The variable
speaks positively because a supervisor setting `..._NO_UPDATE_MANIFESTS=0` is
being asked to read a double negative; anything falsey in it turns the checks
off, and the flag on the command line wins over it in the usual way. It joins
the list in "Configuration is flags, with an environment variable behind each"
above, and follows that rule so that there is one way to configure a daemon.

### The first check is a minute late, and the rest are half an hour apart

**Sixty seconds, then thirty minutes.** The delay is there because a fleet
restarted together would otherwise arrive at the catalog as one crowd, and
because a daemon that is started and stopped inside a minute — which is what
most of this program's own tests do — should not reach the network at all. Half
an hour is far finer than the thing being watched, which changes on the scale of
days. Both, and the catalog location, are one value a caller can replace, so
that anything pointing a daemon at a catalog of its own gets to say how often it
is read as well as where it is.

## 2026-08-21 — saying which generation of the hooks a machine carries

### A count per agent, in a comment at the top of every file installed

**One `u32` per agent, written into the file as `AGENTBUS_HOOK_VERSION=<n>` and
compared with `>=`.** An installation that is either present or absent cannot
answer the question a user actually has, which is whether what they installed
months ago is what this build would install today. Every file `agentbus install`
writes whole now says which generation it is, in a comment among its opening
lines, and status is a read of that one line.

It is deliberately not a version of anything. Nobody releases it, nothing
depends on a particular value, and no ordering beyond "at least" is ever asked
of it — so the two extra numbers a semantic version carries would be two extra
things to get wrong for no question they answer. One count per agent, not one
for the program: the agents change independently, and rewriting the wrapper one
of them runs is no reason to tell everybody else's user that their hooks are
behind. A file marked *newer* than this build writes counts as current, because
a machine somebody has already upgraded is not one an older build should talk
into installing over it.

The count is read from the installed file, never from this program's record of
what it wrote. The file is what the agent runs, a user can read it without this
program's help, and a record that disagreed with it would be a confident answer
about the wrong machine — one restored from a backup, one copied from somewhere
else, one somebody has edited. The record grows a note of each agent's installed
paths and generation all the same, in a second revision of its schema that reads
the first one whole: that is bookkeeping for the questions the files cannot
answer, such as where a build that has since been replaced put things.

A mark is only honoured in the comment at the top of a file — the first line
that is neither blank nor a comment ends the search. Further down it would be
something a file could acquire by accident, from a string or a heredoc or a line
of somebody else's, and this decides whether a user is told their hooks are
current. A file with no mark at all is reported as out of date rather than as
unknown: that is what everything installed before this scheme looks like, what a
document with nowhere to put a comment looks like, and the fix for both is the
same one.

### Windows files ship before the Windows client does

**The installer writes PowerShell wrappers on a Windows machine, while the emit
client and the daemon remain unported.** Installing is the half of this program
that can be made to work on both kinds of machine cheaply — it is paths, files
and a little quoting — and holding it back until the rest follows would mean
writing all seventeen agents' installers twice, once now and once again later.

What lands on such a machine in the meantime does nothing, and does it safely: a
wrapper whose first act is to look for the binary it hands events to finds
nothing there and exits reporting success. That is not a special case for the
occasion — it is exactly what every wrapper does on a machine where the binary
has been removed, and what the emit path itself does when no daemon is
listening. An installation ahead of its client is therefore inert rather than
broken, and becomes live the day the client arrives, without anybody having to
install again.

The one thing this costs is that the encoded spelling of a PowerShell command —
base64 of UTF-16, for the hook runners that put what they are given through a
shell of their own — is built here rather than taken from a library. It is one
table and one loop used in one place, and a dependency for it would be more to
keep an eye on than to keep.

## 2026-08-21 — editing a file somebody keeps

### A concrete syntax tree for the two configuration files people hand-edit

**`jsonc-parser`, pinned to an exact version, with its `cst` and `serde_json`
features and nothing else.** This is the first dependency this workspace has
taken for a job it could in principle do itself, so it is worth saying what it
buys. Most of the files `agentbus install` writes into belong to an agent: it
generates them, this program adds to them, and nobody reads them for pleasure.
Rewriting one whole from a parsed value is right for those, and that is what the
existing merge path does.

Two of them do not belong to an agent. They are files people open in an editor
and keep, with comments in them, sections written on one line because that is
how their author likes them, and whatever line endings their machine uses. A
whole-file rewrite of one of those is a config file quietly reformatted and a
diff nobody asked for, and the loss is not only cosmetic — a rewrite cannot
carry a comment at all, because the value it parsed never held one.

Editing text in place needs a parse that keeps the punctuation and the
whitespace as well as the values, and that is a fair amount of code to get
right: escapes, comment placement, where a comma goes. Writing it here would be
a JSON parser this repository maintains in order to add two lines to two files.
The exact pin is because the thing being depended on is not an API but an
output — how the tree renders is what lands in a user's file — and that is not
something to let float.

### Every edit is checked by reading back what it wrote

**The desired value is worked out first, and an edit whose text does not parse
back to exactly that value is refused.** Splicing text is guesswork in a way
that serializing a value is not: where the comma goes in a one-line object,
which side of a trailing comma a new element belongs on, whether the comma found
before the closing bracket is a real one or one inside a comment. Rather than
try to be right about every such case, each edit states what the document is to
mean, makes the change, reads the file back and compares. A disagreement of any
kind is a refusal that names the file and changes nothing, and the planner turns
it into a failed plan.

That check is also what makes the shortcut underneath it safe. An edit whose
desired value is the value the document already holds writes no text at all and
hands back the original bytes, so installing twice, or taking out exactly what
was just put in, leaves a file with the modification time it had before anybody
looked at it.

## 2026-08-21 — the files that are not JSON

### The TOML and YAML editors are written here, not taken from a crate

**Hand-rolled line editors, in `crates/install/src/toml_text.rs` and
`crates/install/src/yaml_text.rs`, rather than `toml_edit` or a YAML
serializer.** This is the opposite call from the one made for JSON a few
sections above, so it is worth saying why the same reasoning lands elsewhere.

What the installer needs from these two formats is small and fixed: a block of
tables between two marker comments, one boolean under one section, and one name
in one list. What a full-document library gives back is the whole file, and for
YAML that means a file whose comments are gone, whose quoting style has been
normalized and whose ordering is whatever the serializer felt like — the same
loss the concrete-syntax-tree parser was taken on to avoid, at a size where the
parser's own bulk is not justified. `toml_edit` would preserve more than that,
but it is still a whole-document model taken on for one boolean and one block.

The narrowness is the safety feature, not a shortcut around one. Each editor
knows a short list of shapes and refuses everything else by name: a section
written twice, a key whose value carries on to the next line, a marker sitting
inside a multi-line string, a YAML list written on one line, an anchor, a tag, a
tab where the indentation belongs. A refusal costs a plan and leaves the file
untouched, which is the outcome a person can fix; a rewrite that guessed would
cost them their configuration. A library, by contrast, would happily accept all
of those and hand back something reformatted.

The pieces this does generate go through one function each — a TOML basic string
with every escape the format defines, and a YAML scalar quoted whenever a reader
would take it for a number, a date or a boolean — so that a value from the
machine this is running on cannot break out of what it is being put between.

### An edit that added a line records nothing except what it had to create

**The two editors are string-to-string functions; the one thing they report
alongside the text is whether they had to write the container as well as the
entry.** Everything else about an install can be worked out again by reading the
file: which line the block is on, whether the key is there, whether the name is
in the list. One thing cannot — a `[features]` holding nothing but this
program's flag, or a `plugins.enabled` holding nothing but its plugin, looks
exactly the same whether this program wrote the container or found it empty and
filled it. So the install reports it, the caller records it, and the uninstall is
handed it back; without it, taking a hook out would either leave a section
behind for ever or take away one somebody else wrote.

Everything else about the round trip is arithmetic on lines. Each line carries
the terminator it arrived with, so a file written on a Windows machine leaves
with `\r\n` and a file that ended without a newline still does; and the blank
line put in front of an appended block comes back out with it, so that an
uninstall over a file this program has not otherwise touched gives back the
bytes the install was handed.

## 2026-08-21 — Claude Code moves onto its settings file

### The generated marketplace is retired, and replaced by a wrapper and one entry

**This supersedes *Claude's plugin is offered through a generated marketplace of
one*.** `agentbus install --agent claude` now writes a wrapper script into
`~/.claude/hooks/` and adds one entry to `~/.claude/settings.json` that runs it.
Nothing is generated below the data directory, and nothing is asked of `claude
plugin` on an ordinary install.

The mechanism was not wrong; it stopped being worth what it cost once there were
seventeen agents to install for rather than three.

- **One mechanism, one status story.** Every other agent is a versioned file
  plus a surgical edit of a file the agent already reads. Claude was the only
  one whose installation lived somewhere else and was registered by running
  somebody else's program, so it was the only one that needed its own reasoning
  in every part of the installer — planning, reporting, staleness, uninstalling.
- **A version-marked asset can be read back.** An installation now says which
  generation it is, in a comment in the file the agent runs, and a status
  command reads that off the machine. JSON has nowhere to put such a comment, so
  the marketplace's `hooks.json` could never carry one; an installation made
  that way is present or absent and nothing else.
- **No install-time dependency on another program's command line.** The old
  sequence depended on the `claude plugin` subcommands keeping their names, on
  Claude being on the `PATH` at install time, and on a subprocess per install
  even when nothing needed doing. A machine whose Claude has since been removed
  could not be installed for at all.

What it costs is the thing the old entry was chosen to avoid: this program now
writes into a file the user maintains by hand. That is paid for by editing it
through the concrete-syntax-tree editor rather than rewriting it — every byte
outside the four lines added comes back exactly as it went in — and by the same
rule every other agent's config file gets, that a file which cannot be written
back safely stops the plan and is left alone.

Machines carrying the old installation are cleaned up by both `install` and
`uninstall`: the generated directory goes, the records of it go, and Claude is
asked to forget the plugin and the marketplace. That runs only when something of
it is actually found — on disk, or in this program's own record, which is the
only thing that still names a file somebody deleted by hand. An answer from
Claude is not required: where it cannot be run, the files go anyway and the
registration is left for the user, which is the half of the job this program can
finish on its own.

### Install-time hooks carry session identity, not the whole event surface

**One event is hooked — `SessionStart` — rather than every event the agent
offers.** The earlier arrangement registered eleven, on the reasoning that
capturing everything at install time meant never having to install again to see
a new kind of event.

What an install actually needs from the agent is the identity of the session in
front of it: which session this is, and where its transcript lives. That arrives
on the first event of the session and does not change afterwards. Everything
else — what the agent is doing, whether it is waiting, whether it has stopped —
is decided from the payload the wrapper forwards, by mappings the binary
carries, and those can be changed by shipping a new binary rather than by
editing seventeen users' configuration files.

Against that, a hook is not free. It is an entry in a file the user reads, and a
process started inside their session every time the event fires. Registering
eleven of them to use one is a cost paid on every tool call for events nothing
is waiting on.

The hook-mapping manifests are deliberately **not** trimmed to match. Mapping is
data: it says what an event means if one arrives, and it costs nothing to know
about events this build does not ask for. Payloads for unhooked events simply
stop arriving, and an installation that hooks more of them later needs no change
to the mappings at all.

## 2026-08-21 — Codex moves onto a wrapper, and one key in `config.toml`

### The setting that makes hooks work is switched on, in the file it lives in

**This supersedes *Codex is installed by dropping into `hooks.json`, never into
`config.toml`*, in one respect and no other.** The entries still go into
`hooks.json`; what has changed is that `agentbus install --agent codex` now also
ensures `hooks = true` under `[features]` in `~/.codex/config.toml`.

The old entry was written when that file was purely a matter of taste. It is
not: a current Codex does not read `hooks.json` at all until the setting is on.
Without it the install writes the right bytes to the right paths, reports
success, and produces nothing for the rest of the machine's life — which is the
worst failure this program can have, because it looks exactly like a working
installation and there is nowhere for the user to see otherwise.

Against that, the edit is one key. It goes in through the line-by-line editor,
which changes the line it is changing and copies every other byte of somebody's
file through untouched, and which refuses the whole run rather than guess at a
file it cannot read that way — a section written twice, a value that carries on
past its line, a marker inside a multi-line string. So the envelope of what this
program writes into a hand-kept file grew by exactly one line, in a file it
already had to read.

Whether the setting was *already* on is the one fact about it that cannot be
read back off the disk afterwards: `hooks = true` looks the same whoever wrote
it, and there is nowhere in a line like that to hang the mark this program's
entries in a document carry. So it is recorded when it is known, as one more
thing the record answers that the files cannot, and an uninstall switches the
setting off only where the record says this program switched it on. A setting
the user had on before is left exactly as it was found, and so is a `[features]`
section they wrote.

### Codex is installed the way every other agent is

**A versioned wrapper script in `~/.codex/hooks/`, and one entry that runs it.**
Codex used to be the one agent whose hooks named this program's binary directly,
in eleven entries, one per event the mapping reads. Both halves of that are now
what the rest of the agents get, for the reasons recorded against Claude the
same week: a JSON entry has nowhere to carry a generation mark, so an
installation made of entries alone can only be present or absent, and hooking
every event is a process started inside somebody's session on every tool call
for events nothing is waiting on. What the install needs from Codex is the
identity of the session in front of it, which arrives on the first event of the
session.

The eleven entries an earlier build wrote need no migration code. They carry the
mark, and a marked merge takes out everything marked before it puts the new
entry in, so upgrading finds them and replaces them exactly as it was always
going to.

The hook mapping is deliberately not trimmed to match. Mapping is data: it says
what an event means if one arrives, and payloads for events this build does not
ask for simply stop arriving.

## 2026-08-21 — the agents configured by a nested `hooks` object

### One installer, four descriptions

**The agents whose settings file holds a `hooks` object of event names share a
single installer, parameterized by what differs.** Four of them are configured
identically — a JSON settings file, a `hooks` key, an object of event names, each
an array of `{matcher?, hooks: [{type, command, timeout}]}` entries — and what
varies is a path, a list of events, whether the entry carries a matcher, and one
agent's choice to read the timeout in milliseconds rather than seconds.

Four modules would mean four copies of the same care about backups, marks,
refusals, ordering and reversal. They would not stay four copies: a fix applied
where it was noticed and nowhere else is how a family of near-identical
installers turns into a family of subtly different ones, and the differences that
matter here are invisible until somebody's session is slower or quieter than it
should be. So there is one installer and four descriptions of an agent, and a
fifth agent configured this way is a fifth description.

The line to hold is what goes into the description. Paths, event tables and field
units belong there. If a fifth agent needs something the shape itself does not
have — a differently-nested key, a second file, an entry that is not an entry —
that is a different idiom wearing this one's clothes, and it gets its own module
rather than a fifth field nothing else sets.

### A configuration directory that is not there is a refusal

**These installers never create the agent's own configuration directory.** Where
Claude, Codex and OpenCode get theirs made for them, a `~/.factory` that does not
exist means Droid has never run on this machine, and the install stops and says
so.

The difference is what the directory is evidence of. For the agents this program
has installed for longest, the config directory is somewhere hooks are dropped
and the agent is known to be present by other means. For these four, its absence
is the only signal available that the agent is not installed — and creating it
anyway would mean guessing at another program's layout, writing a settings file
into a directory that program may never read, and leaving this program answerable
for a directory it invented. Saying "install the agent first" costs a user one
line and nothing else.

## 2026-08-21 — the agents that hand over a region of their own

### Ownership is claimed by region where an agent offers one

**Where an agent lets a tool have a whole key or a whole file, that region is
the mark.** Everywhere else, this program finds its own work again by the key it
writes into every entry, because its entries sit in an array beside the user's
and nothing else could tell them apart. Two agents make that unnecessary.
Antigravity keys its hooks file by the name of a *hook* rather than by the name
of an event, so everything one tool registers hangs below one key of that tool's
choosing; Grok merges every JSON file in a directory, so a tool can own a file
outright and never open a shared one.

Taking those offers costs nothing and buys the simplest installs here: install
writes the region, upgrade rewrites it whole, uninstall removes it, and no other
key or file is read, written or counted. It also avoids a real risk — a mark is
a key somebody else's schema did not ask for, and an agent that validates its
hooks strictly would be within its rights to reject a document carrying one.

What the record still has to hold is which files this program *created*, exactly
as before: an empty hooks file this program made is litter and one it merely
added a key to is the user's, and nothing on disk can tell those apart.

### The event's name may come from the command line

**`agentbus emit --event <name>`, and a mapping that names no event field.**
Every agent here but one puts the name of the event in the payload, and the
mapping reads it out of a field the manifest names. Antigravity does not: it
delivers one payload shape per event and expects whoever registered the hook to
remember which event they registered for.

Three ways to bridge that were available, and the payload rule decided between
them. The wrapper could have edited the payload on its way past — but a wrapper
that adds a field is a wrapper putting this program's words in the agent's
mouth, and the mapping would then be written against a shape nobody documents.
The mapping could have named a field the payload does not have and quietly never
fire. Instead the name travels *beside* the payload: the entry written into the
agent passes it to the wrapper, the wrapper passes it to `emit`, and what is on
standard input reaches the far end exactly as the agent wrote it.

The payload wins wherever it answers. A name from a command line is a fact about
which hook was run; a payload that names its own event is the agent itself
speaking, and somebody reading a manifest against a captured payload should be
able to work out what will happen from the payload alone.

A manifest that names no event field is new expressive power rather than an
omission, so it is behind a hook-engine version: an older engine refuses such a
manifest outright instead of half-reading it.

### One wrapper answers, because silence is an answer there

**Antigravity's wrapper prints an empty object and nothing else.** Every other
wrapper here says nothing at all on standard output, because the agents read
what a hook prints as an instruction and this program has none. Antigravity
reads a hook's standard output as a list of steps to insert into the user's
session, and an empty object is how "no steps" is spelled. Saying nothing there
is not the same as saying nothing to insert — it is leaving the agent to make
what it will of silence.

So the rule stands and the exception is written down: the wrapper prints exactly
that object, on every path out of it, and prints nothing else ever.

## 2026-08-21 — Kimi, and the agent that can be too old

### Kimi's tables live in a fenced block inside the user's `config.toml`

**`[[hooks]]` tables go between two marker comments in `~/.kimi-code/config.toml`,
and everything outside them is copied through byte for byte.** Kimi has no
drop-in directory and no separate hooks file: the tables belong in the same
file the user keeps their model, their approvals and their comments in. So the
rule that governs every hand-kept file here governs this one, and the claim on
what this program wrote is made by position — a TOML table has nowhere to hang
the key that marks this program's entries in a JSON document.

That makes the marker lines load-bearing in a way they are not elsewhere, and
the line-by-line editor treats them accordingly: two opening markers, an opening
without a closing, or a marker inside a multi-line string all refuse the run
rather than produce a guess. An uninstall gives back the bytes the install was
handed, blank line included.

### The whole of the registered surface goes in, matchers and all

**Twelve rows, three of them narrowed by one of Kimi's own regular expressions,
and every row running the same wrapper.** Two of the three are the two halves of
`PreToolUse` — the tool that puts a question to the person at the keyboard, and
every other tool — which together are that event entire. With one command in
every row the pair is behaviourally identical to a single unnarrowed row, and it
is still written as two: the distinction is one the agent draws, and collapsing
it would be this program deciding at install time that a distinction the agent
makes is not worth carrying into somebody's configuration file.

The expressions are Kimi configuration, written for Kimi to evaluate. None of
them is ever read by this program, and nothing on the path an event takes from
the wrapper to the bus matches anything — the mappings that path is driven by are
tables of names, by decision, and an expression arriving out of a configuration
file is exactly what that decision keeps out of it.

### An agent too old for its hooks is refused, and one that cannot be asked is not

**`kimi --version` is run while the plan is being worked out, and its three
answers are three different outcomes.** Kimi is the one agent here whose hooks
arrived in a known release, and installing into an older one writes the right
bytes to the right paths and produces nothing — the same failure the Codex
setting exists to prevent, arriving from the other direction. So a version below
the floor makes the plan a refusal that names both versions, before any file is
touched.

The unanswerable case is deliberately not a refusal. No command on the search
path, a command that fails, output with no version in it: all of them mean *this
program does not know*, and a user whose agent could not be interrogated is
better served by working hooks and a sentence about them than by neither. The
probe is therefore a question rather than a step — it is allowed to fail, and
failing is not an error — matching how every other command this program runs to
find something out behaves.

### A plan step that changes nothing and says something

**`Change::Note` carries a remark through the plan to the report.** The warning
above has to reach the user on the dry run as much as on the real one, in order
among the files it is about, and both of those follow from travelling the same
way every other part of a plan travels. The alternative — a channel of its own,
outside the plan — would print out of order at best and not at all on a dry run
at worst, which is the run the remark matters most on. It changes nothing on the
machine, like the step that records who a setting belongs to, and an agent whose
whole plan is one remark is still correctly reported as having nothing to do.

## 2026-08-21 — asking a machine what its hooks are

### The question gets a noun of its own

**`agentbus hooks status`, not a flag on `agentbus status`.** `status` already
means the sessions the bus knows about, which is the live question a person
asks all day; what is installed in the agents on a disk is a different subject
that happens to want the same word. Rather than overload it, the thing that was
installed becomes the noun and the question hangs off that, which leaves room
for anything else worth asking about installed hooks later. `install` and
`uninstall` stay where they are: they are what everybody already types, and
moving them under the new noun would be renaming a command to tidy a menu.

The report is one line per agent, and every agent this program knows is on it
whether or not it is on the machine — a report that silently omitted what it
had nothing to say about could not be trusted to be complete. Four answers, and
each names the next thing to do: current, behind this build, current but run by
nothing, and absent. An absent agent that is nevertheless *on* the machine is
the one case where the line ends in the command that would fix it, because that
is the only one where the reader has a decision to make rather than a repair.

### The file that answered is named, and where nothing answered nothing is named

**Every state but "not installed" ends in the path the reading came from.**
Somebody told their hooks are old, or that what is there never runs, is about to
go and look at it, and the path is the whole of what they need in order to. An
agent with nothing installed has no such file, and printing where one *would* go
would be describing an installation that does not exist. So the installers grew
one more thing they answer for — which of their files carries the mark — beside
the reading of it they already did.

### An install mentions the agents it was not asked about

**One sentence on stderr, after a successful install, naming the other agents
whose hooks a newer build has left behind.** Somebody who installs for one agent
has said which agent they care about now, not that the rest of their machine has
stopped mattering, and the moment they are already thinking about hooks is the
cheapest one they will ever get to hear that the others are behind. It goes to
stderr because it is not part of the account of what this run did to the agents
it was asked about.

It is deliberately a narrower question than the one the report answers: agents
that have *no* hooks are never mentioned, because having none was somebody's
choice and a command that has just finished doing what it was told is no place
to argue with it. Uninstalling says nothing at all, for the same reason.
