# agentbus and Shepherd

**`agentbus`** is a generic event bus for coding agents. Agents report what they
are doing through hooks; the daemon folds those events into the current status of
each session and publishes them to any subscriber over a Unix socket, including
sessions running inside containers or on remote hosts. It is a single small
binary, useful on its own from a shell script or a terminal.

**Shepherd** is a GUI terminal multiplexer that consumes the bus: panes, tabs and
workspaces that show at a glance which agent is waiting for you. It is a separate
application and a later deliverable; `agentbus` neither knows nor cares that it
exists.

Technology choices and the reasoning behind them are recorded in
[`docs/decisions.md`](docs/decisions.md).

## Running Shepherd on macOS

macOS only treats a program as an application when it is inside a bundle, so
Shepherd is built into one. A Mac is also the only machine that can build it at
all — the toolkit compiles its shaders with Xcode's own tools on the machine
doing the building.

```sh
scripts/bundle-macos.sh              # dist/Shepherd.app, for this Mac
open dist/Shepherd.app               # or double-click it in Finder
```

The bundle is ad-hoc signed, which is enough for the machine that built it. It is
not notarised, so it is not something to pass on to anybody else.

It also carries the bus's binaries for the machines a Mac is not, so that
Shepherd can put a bus inside a container without fetching anything. They are
taken from `dist/`, or from `--assets DIR`, and are built wherever a Linux
toolchain is at hand:

```sh
scripts/build-release.sh aarch64-unknown-linux-musl
scripts/build-release.sh x86_64-unknown-linux-musl
```

A bundle built without them is still a bundle; it fetches what it needs from a
published release instead, and says so as it is assembled.
